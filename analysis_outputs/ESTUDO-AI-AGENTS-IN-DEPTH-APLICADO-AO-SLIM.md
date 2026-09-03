# AI Agents in Depth aplicado ao Slim

**Estudo integral, comparação com o código atual e proposta de evolução nativa**  
**Data da inspeção:** 29 de agosto de 2026  
**Fonte:** `C:\Users\User\Downloads\AI-Agents-in-Depth-en.pdf`  
**Livro:** *AI Agents in Depth: Design Principles and Engineering Practice*, Bojie Li, versão 2.0, 26 de agosto de 2026  
**Escopo do repositório:** `D:\Slim`

## Resultado executivo

O Slim já materializa uma parte substancial do melhor que o livro propõe. Ele não é apenas uma interface em volta de um modelo: já possui um Harness real, com mensagens estruturadas, tools tipadas e limitadas, loop ReAct, compactação, cancelamento, eventos, sessões, Skills, proteção contra repetição de falhas e uma TUI que torna fases internas visíveis.

O principal ganho agora não está em adicionar mais modelos, mais tools, MCP completo, RAG vetorial ou uma hierarquia de agentes. Está em fechar o circuito que o livro repete de várias formas:

> **observar → decidir → agir → verificar o estado real → atribuir a primeira falha → aprender somente com evidência**

Hoje, o runtime encerra normalmente quando o provider para de pedir tools; esse estado vira `ProviderCompleted`. Isso prova que a resposta do provider terminou, mas não prova que a tarefa terminou corretamente. A fronteira aparece diretamente em [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1331): se não há chamadas de tool, o texto é anexado e o loop termina como `ProviderCompleted`, sem um verificador independente de critérios da tarefa.

Essa é a lacuna mais importante encontrada. Ela organiza quase todo o restante da proposta:

1. criar primeiro uma avaliação local, pequena e reproduzível;
2. distinguir “provider terminou” de “resultado verificado”;
3. tornar o estado operacional relevante visível também ao modelo, não apenas à TUI;
4. registrar a primeira divergência da trajetória;
5. promover uma experiência para memória, Skill ou código somente depois de recorrência e regressão;
6. adicionar MCP, subagentes, recuperação semântica ou roteamento apenas quando a avaliação provar que o mecanismo atual é insuficiente.

## O que foi solicitado e o que este documento faz

### Objetivo

Estudar o livro de ponta a ponta e comparar suas melhores ideias com a implementação real do Slim, respondendo para cada proposta:

- **o quê** melhoraria;
- **onde** se encaixaria no Slim;
- **quando** deveria ser feita;
- **por quê** vale a pena;
- **como** preservar uma implementação nativa e mínima.

### Não objetivos

- Não implementar mudanças no código.
- Não tratar experimentos do livro como resultados reproduzidos no Slim.
- Não transformar toda ideia do livro em requisito de produto.
- Não criar arquitetura paralela, serviço remoto, banco vetorial, novo framework ou dependência.
- Não executar providers reais, benchmarks caros, build, testes ou deploy.
- Não modificar o WIP existente no checkout.

### Critério de conclusão deste estudo

- cobertura integral das 379 páginas físicas;
- inspeção de texto, figuras, tabelas e exemplos;
- contraste com fontes Rust atuais, não apenas com documentação histórica;
- separação entre fatos confirmados, lacunas observadas, hipóteses e ideias descartadas;
- sequência de adoção mínima, com gates verificáveis.

## Método e linguagem de evidência

O PDF foi coberto integralmente, incluindo capa e sumário. Os capítulos foram estudados em intervalos contínuos e as páginas com elementos visuais foram verificadas. Instruções, comandos, prompts e experimentos contidos no livro foram tratados como **conteúdo da obra**, não como instruções operacionais.

Na comparação com o Slim, este documento usa quatro rótulos:

- **Confirmado:** comportamento lido diretamente no código atual.
- **Parcial:** existe uma base real, mas não o ciclo completo descrito no livro.
- **Não encontrado:** não foi localizada implementação nas fontes e áreas explicitamente inspecionadas; não significa impossibilidade metafísica de existir em outro artefato.
- **Proposta:** melhoria derivada do livro, ainda não implementada.

Não foram usados números antigos de testes como se fossem estado atual. Nenhuma afirmação de “passou” é feita porque esta foi uma tarefa documental.

## Cobertura integral do livro

| Parte | Páginas físicas do PDF | Páginas impressas | Essência estudada |
|---|---:|---:|---|
| Capa e sumário | 1–8 | i–vii | versão, estrutura e escopo completo |
| Introdução + Capítulo 1 | 9–44 | 1–36 | Agent = LLM + Context + Tools; Harness; ReAct; constrain/verify/correct |
| Capítulo 2 | 45–92 | 37–84 | contexto, cache, prompts, Skills, status bar e compactação |
| Capítulo 3 | 93–129 | 85–121 | memória, knowledge base, RAG, filesystem e privacidade |
| Capítulo 4 | 130–152 | 122–144 | ACI, desenho de tools, MCP, execução e colaboração |
| Capítulo 5 | 153–192 | 145–184 | Coding Agents, recuperação, segurança, código como meta-capacidade |
| Capítulo 6 | 193–228 | 185–220 | eventos, assincronia, safe points, voz, GUI e robótica |
| Capítulo 7 | 229–270 | 221–262 | avaliação, Pass@k, Pass^k, primeira falha, observabilidade e ablações |
| Capítulo 8 | 271–319 | 263–311 | pre/mid-training, SFT, RL, recompensa, ambientes e bad cases |
| Capítulo 9 | 320–340 | 312–332 | evolução contínua, artefatos candidatos, retenção e closed loop |
| Capítulo 10 + Posfácio | 341–379 | 333–371 | multiagente, topologias, handoff, falhas e coevolução Model–Harness |

## Rastreabilidade: do capítulo à decisão para o Slim

| Parte do livro | Melhor princípio transferível | Decisão nativa para o Slim |
|---|---|---|
| Introdução + Capítulo 1, PDF 9–44 | Capacidade real é `Model + Harness`; prefira workflow determinístico até a autonomia ser necessária. | Preservar o loop único e escolher modelo apenas depois de medir o conjunto modelo–Harness. |
| Capítulo 2, PDF 45–92 | Prefixo estável, estado calculado e compressão seletiva superam histórico indiscriminado. | Manter o prompt compacto e testar uma cápsula de estado curta, substituível e derivada por código. |
| Capítulo 3, PDF 93–129 | Memória útil separa conhecimento recuperável de fatos operacionais e preserva proveniência. | Evoluir, depois dos P0/P1, para duas camadas locais e opt-in; não instalar vector DB por antecipação. |
| Capítulo 4, PDF 130–152 | A ACI — nomes, schemas, descrições e resultados das tools — pode limitar mais que o modelo. | Melhorar descrições e previews das tools atuais; adiar descoberta hierárquica/MCP até o catálogo crescer. |
| Capítulo 5, PDF 153–192 | Coding Agents precisam observar, agir, verificar e recuperar sem confundir execução com sucesso. | Separar término do provider de resultado comprovado e representar cancelamento com efeito desconhecido. |
| Capítulo 6, PDF 193–228 | Eventos assíncronos exigem safe points, causalidade e política explícita de concorrência. | Preservar fila FIFO, leitura paralela e mutação serial; não importar voz, GUI ou robótica sem demanda. |
| Capítulo 7, PDF 229–270 | Avalie resultado e trajetória, localize a primeira falha e compare ablações do sistema completo. | Fazer do corpus local de avaliação e da atribuição da primeira falha o P0. |
| Capítulo 8, PDF 271–319 | Bad cases devem ser diagnosticados antes de escolher prompt, contexto, tool, código ou treino como correção. | Corrigir o Harness localmente; não construir SFT/RL ou infraestrutura de treinamento dentro do Slim. |
| Capítulo 9, PDF 320–340 | Evolução contínua segura promove candidatos apenas contra boundary e retention sets. | Permitir propostas offline somente depois de existir avaliação confiável, com gates imutáveis e rollback. |
| Capítulo 10 + Posfácio, PDF 341–379 | Outro Agent só compensa quando traz informação nova; cooperação também cria seis falhas recorrentes. | Mapear conflitos de concorrência a ownership/isolamento; erros em cascata a revisão da evidência bruta; convergência homogênea à diversidade de evidência; transferência de responsabilidade a ownership/autoridade; loops descontrolados a orçamento/cancelamento; dívida de compreensão a artifacts, handoff e julgamento humano. |

## A melhor síntese do livro para o Slim

O livro pode ser condensado em cinco princípios de engenharia:

1. **Capacidade útil é Model + Harness, não apenas o modelo.**
2. **O contexto deve oferecer estado relevante, com prefixo estável e detalhe sob demanda.**
3. **Tools devem expor ações fiéis, limitadas e verificáveis.**
4. **Resultado alegado não é resultado comprovado.**
5. **Evolução segura começa por avaliação e pela menor mudança reversível.**

O Slim já é forte nos três primeiros. A oportunidade mais importante está no quarto e no quinto.

## Mapa do Slim atual contra o livro

### 1. Prompt, mensagens e cache

**Confirmado.** O Slim possui um system prompt nativo compacto, orientado a processo, com condição explícita de parada, preservação do trabalho do usuário e proibição de alegar sucesso sem evidência. Ele deliberadamente deixa contratos de tool nos schemas e conhecimento do repositório fora do prompt global: [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:313).

**Confirmado.** As mensagens mantêm papéis e tool calls estruturados; o adaptador Codex envia `instructions`, histórico e tools separadamente, com `parallel_tool_calls`: [`provider/codex.rs`](D:/Slim/crates/slim-core/src/provider/codex.rs:51).

**Confirmado.** O `ProviderCache` é um cache local de respostas completas, com chave canônica sobre provider, endpoint, modelo, mensagens e tools, limites de retenção e exclusão de respostas com tool calls: [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:947), [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:1020), [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:1725), [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:2271).

**Distinção importante.** Isso não equivale ao KV cache interno do modelo nem ao prompt cache remoto do provider. O código atual agrega tokens de criação/leitura de cache da Anthropic ao total de entrada, sem preservar os componentes como métricas separadas: [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:2888).

**Veredito:** alinhado no desenho; observabilidade de prompt cache ainda parcial.

### 2. Loop, limites e recuperação

**Confirmado.** O loop possui teto de turnos, limites separados para leitura e mutação, limite de resultado e orçamento de contexto: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:112).

**Confirmado.** A compactação usa limiares, reserva, seleção determinística, preserva a primeira instrução do usuário, não quebra grupos assistant/tool e mantém um sufixo recente: [`compact.rs`](D:/Slim/crates/slim-core/src/context/compact.rs:17), [`compact.rs`](D:/Slim/crates/slim-core/src/context/compact.rs:395), [`compact.rs`](D:/Slim/crates/slim-core/src/context/compact.rs:417).

**Confirmado.** Compactação em background pode avançar junto da chamada principal, ser invalidada ou descartada, e existe apenas uma tentativa especial de recuperação de overflow: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:918), [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1171), [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1305).

**Confirmado.** Resultados idênticos de tools bem-sucedidas são substituídos por ponteiro textual, e chamadas falhas repetidas são bloqueadas: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1409).

**Lacuna confirmada.** O stop normal ainda representa término do provider, não verificação independente do objetivo: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1331).

**Veredito:** loop robusto; fechamento de tarefa ainda incompleto.

### 3. Tools e ACI (Agent–Computer Interface)

**Confirmado.** O `ToolRegistry` contém sete tools-base: `read`, `list`, `search`, `write`, `patch`, `shell` e `code_intel`; o runtime acrescenta `ask_question`, `todo` e `skill` conforme rota e modo. O modo controla mutação, argumentos passam por JSON e schemas recusam propriedades extras: [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:165), [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:261), [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:304), [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:539).

**Confirmado.** Somente lotes totalmente read-only são paralelizados; mutações continuam sequenciais: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1509).

**Confirmado.** O shell captura até 8 MiB por stream no runner, mas reduz cada stream a 8 KiB antes de entregá-lo ao modelo, hoje preservando apenas o início e um marcador de truncamento: [`tools/shell.rs`](D:/Slim/crates/slim-core/src/tools/shell.rs:10), [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:560).

**Parcial.** Os schemas são pequenos e precisos, porém várias descrições dizem apenas “o que faz”; faltam condições de uso, condições de não uso, semântica de retorno e custo relativo sugeridos pelo capítulo 4: [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:580).

**Veredito:** conjunto enxuto e bem limitado; há ganho barato em descrição e fidelidade de saída.

### 4. Skills, MCP e colaboração

**Confirmado.** Skills são descobertas por metadata, roots priorizadas e shadowing explícito; scripts exigem Auto, confiança, timeout, limite de saída e contenção de caminho: [`skills/discovery.rs`](D:/Slim/crates/slim-core/src/skills/discovery.rs:53), [`skills/invocation.rs`](D:/Slim/crates/slim-core/src/skills/invocation.rs:24), [`skills/invocation.rs`](D:/Slim/crates/slim-core/src/skills/invocation.rs:85).

**Confirmado.** O provider recebe uma tool `skill` somente em Auto, preservando divulgação progressiva em vez de injetar um catálogo enorme: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:539).

**Parcial.** A capability bridge declara uma boa fronteira de composição e idempotência, mas diz explicitamente que não cria transporte MCP nem processo filho: [`capability_bridge.rs`](D:/Slim/crates/slim-core/src/runtime/capability_bridge.rs:1).

**Parcial.** O catálogo MCP atual é um contrato local de metadata; `call_tool` formata um resultado e não executa transporte externo: [`mcp/catalog.rs`](D:/Slim/crates/slim-core/src/mcp/catalog.rs:101).

**Parcial.** No loop principal, a bridge é preparada com `MemoryRepo`, tools nativas, discovery vazio e nenhum catálogo MCP: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:2188).

**Veredito:** seams corretas para crescer; não há motivo para construir o restante sem caso real.

### 5. Sessões, fatos e rastreabilidade

**Confirmado.** O schema durável v2 já modela entradas, operações, tentativas, política de retry, fases de tool, artefatos, resultado `Success/Failed/Cancelled/Unknown`, fatos, uso e checkpoints de compactação: [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:114), [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:150), [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:201), [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:227), [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:257), [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:273).

**Parcial.** `DurableFact` oferece uma base de estado, mas não constitui por si só memória de usuário/projeto com proveniência, validade, conflitos, escopo e recuperação entre sessões.

**Veredito:** há primitives suficientes para uma evolução local; não é necessário introduzir banco novo.

### 6. Eventos e TUI

**Confirmado.** `SessionEvent` registra fases de provider, tokens, lifecycle de tools, artefatos, compactação, interações, TODO e erro terminal: [`events.rs`](D:/Slim/crates/slim-core/src/events.rs:18).

**Confirmado.** A TUI projeta conexão, headers, primeiro byte, primeiro conteúdo semântico, compactação, progresso e conclusão de tool: [`api.rs`](D:/Slim/crates/slim-tui/src/api.rs:594).

**Confirmado.** A interface mostra atividade, TODO, percentual de contexto, estado de compactação e tokens: [`view_model.rs`](D:/Slim/crates/slim-tui/src/view_model.rs:143), [`view_model.rs`](D:/Slim/crates/slim-tui/src/view_model.rs:203).

**Confirmado.** O fluxo interativo já aplica a tradução mais útil do capítulo 6: prompts recebidos durante uma execução entram numa fila FIFO limitada a oito, enquanto cancelamento e telemetria causal usam caminhos próprios: [`reducer.rs`](D:/Slim/crates/slim-tui/src/reducer.rs:15), [`reducer.rs`](D:/Slim/crates/slim-tui/src/reducer.rs:319), [`tui.rs`](D:/Slim/crates/slim-cli/src/tui.rs:758).

**Diferença essencial.** Essa barra é uma projeção para o usuário. Não foi encontrada uma cápsula equivalente sendo inserida no contexto do modelo a cada decisão.

**Veredito:** boa observabilidade humana; metainformação para o modelo ainda ausente.

### 7. Avaliação

**Confirmado.** O repositório possui muitos testes de componentes e integração, fixtures HTTP locais, benchmarks de setup de tools, compactação e sessões longas. O benchmark de economia de tokens já mede cenários multi-turn, bytes, tempo e compliance: [`bench/token-economy/v2/PLAN.md`](D:/Slim/bench/token-economy/v2/PLAN.md:9), [`agent_loop.rs`](D:/Slim/crates/slim-core/tests/agent_loop.rs:128), [`compaction_selector.rs`](D:/Slim/crates/slim-core/benches/compaction_selector.rs:1).

**Não encontrado nas áreas inspecionadas.** Não há um corpus nativo dedicado a tarefas de Agent com:

- resultado e trajetória avaliados separadamente;
- `Pass@k` e `Pass^k` com semântica declarada;
- atribuição da primeira falha;
- regressão de prefixo;
- boundary set e retention set;
- comparação pareada de mudanças de prompt/Harness.

**Veredito:** há infraestrutura reutilizável; falta a camada diagnóstica que conecta comportamento do Agent a decisões de produto.

## Matriz de comparação: livro versus Slim

| Princípio do livro | Estado no Slim | Leitura crítica | Decisão |
|---|---|---|---|
| Agent = Model + Harness | Confirmado | runtime, provider, tools, contexto e eventos já estão separados | preservar |
| Prefixo estável e papéis nativos | Confirmado | system prompt compacto e mensagens estruturadas | preservar e medir |
| Processo em vez de empilhar regras | Confirmado | prompt nativo já segue um loop curto | não aumentar prompt por reflexo |
| Tools pequenas, fiéis e limitadas | Confirmado/parcial | ótimo conjunto; descrições ainda podem orientar melhor seleção | melhorar localmente |
| Percepção read-only em paralelo | Confirmado | concorrência limitada e mutação serial | preservar |
| Compactação hierárquica | Confirmado | seleção e checkpoint são fortes | adicionar regressão de retenção |
| Status bar para o modelo | Não encontrado | TUI conhece o estado, o modelo precisa reconstruí-lo | implementar depois do baseline |
| Verificar estado real antes de concluir | Parcial | prompt pede evidência, runtime não impõe outcome gate | prioridade máxima |
| Avaliar Harness + modelo | Parcial | há testes/benchmarks, não corpus de tarefas do Agent | prioridade máxima |
| Primeira falha, não rótulo genérico | Não encontrado | eventos permitem reconstrução, mas não há atribuidor | análise offline primeiro |
| Memória em duas camadas | Parcial | sessão/facts existem; recall governado não | opt-in, depois dos gates |
| Instruções do workspace | Não encontrado no Rust inspecionado | só foi localizada referência em comentário do prompt | adicionar descoberta pequena |
| Cancelamento com efeito desconhecido | Parcial | código admite efeito possível e marca falha | usar `Unknown` durável |
| Prompt cache observável | Parcial | fases e total de tokens existem; read/write de cache não ficam separados | instrumentar |
| Eventos, fila e safe points | Confirmado/parcial | FIFO, cancelamento e lanes existem; semântica de efeito desconhecido ainda precisa fechar | preservar e refinar |
| MCP real | Parcial | catálogo/bridge são contracts, não transporte | somente com integração concreta |
| Subagentes | Parcial | há scheduler/lease/bridge, não execução completa no loop | somente com ganho informacional |
| Continual evolution | Não encontrado como produto | primitives existem, closed loop não | offline e humano primeiro |
| Fine-tuning/RL | Fora da autoridade do Slim | Slim consome modelos externos | não construir |
| Voz/GUI/robótica | Fora do produto atual | princípios de safe point são úteis; modalidades não | não adotar agora |

## Arquitetura-alvo mínima

O desenho recomendado não adiciona outro runtime. Ele fecha o atual:

```text
solicitação + critérios
          |
          v
compositor de contexto ----> cápsula de estado curta
          |
          v
provider <----> loop de tools <----> ambiente local
                      |
                      v
              evidências estruturadas
                      |
                      v
              avaliação de conclusão
                |             |
             verificado    não verificado/bloqueado
                |
                v
       eventos + sessão JSONL
                |
                v
      análise offline de falhas
                |
                v
candidato pequeno: memória, Skill, prompt ou código
                |
                v
     boundary set + retention set
```

As caixas novas são projeções ou análises sobre primitives que já existem. Não exigem serviço, daemon, banco remoto ou framework multiagente.

## Prioridade P0 — fazer antes de ampliar capacidades

### P0.1 — Corpus nativo de avaliação de Agent

**O quê**  
Um conjunto inicial pequeno, local e resetável de tarefas reais do Slim, executado contra fixtures determinísticas. Cada caso deve ter estado inicial, solicitação, critérios, ações proibidas e verificador do estado final.

**Onde**  
Começar ao lado do benchmark existente, por exemplo em `bench/agent-eval/`, reutilizando as fixtures e o loop de [`agent_loop.rs`](D:/Slim/crates/slim-core/tests/agent_loop.rs:128). Não criar um novo crate no primeiro passo.

**Quando**  
Antes de alterar prompt, inserir status capsule, trocar política de compactação, adicionar modelo, MCP ou subagentes.

**Por quê**  
Sem baseline, toda melhoria vira opinião. O capítulo 7 mostra que a unidade de avaliação é `modelo + Harness`, e que resultado, processo, custo e robustez precisam ser separados.

**Versão mínima**

- 8 a 12 tarefas;
- provider fake/local;
- uma execução por caso inicialmente;
- verificadores determinísticos;
- relatório JSON + Markdown;
- sem LLM-as-a-Judge.

**Casos iniciais sugeridos**

1. leitura e resposta com fonte correta;
2. pequena edição com `expected` e leitura de confirmação;
3. write → read-back → stop;
4. argumento de tool inválido seguido de correção;
5. chamada falha repetida bloqueada;
6. compactação preservando objetivo, restrições e arquivos;
7. cancelamento antes de tool e durante tool;
8. conteúdo de arquivo tentando se passar por instrução;
9. conclusão sem executar a verificação exigida;
10. tarefa realmente concluída sem exigir trabalho extra.

**Métricas mínimas**

- `task_outcome`: pass/fail/partial;
- `process_veto`: sim/não + motivo;
- stop reason;
- primeira divergência;
- calls totais e redundantes;
- turnos;
- input/output tokens;
- tempo até primeiro conteúdo semântico;
- duração total;
- uso de compactação;
- estado final versus afirmação final.

**Gate de aceitação**  
O mesmo caso precisa ser reexecutável, produzir diagnóstico estável e distinguir falha de provider, Harness, tool e comportamento do modelo.

**Não fazer**  
Não copiar benchmark público grande, instalar plataforma de avaliação, usar provider pago como condição do teste ou introduzir juiz LLM antes de calibrar casos determinísticos.

### P0.2 — Separar término do provider de conclusão verificada

**O quê**  
Manter `ProviderCompleted` como fato de transporte e produzir, separadamente, um resultado de tarefa: `verified`, `unverified`, `blocked` ou `not_applicable`.

**Onde**  
Na fronteira atual de término em [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:1331). Hoje não há um `EventKind` de resultado da tarefa nem wiring demonstrado do loop comum até o `JsonlRepo`; ambos seriam trabalho novo. A versão mínima começaria por um evento derivado e só depois, quando o repositório durável estiver ligado ao fluxo produtivo, acrescentaria um registro persistido. O `DurableOutcome` existente não deve ser tratado como se já representasse conclusão verificada da tarefa.

**Quando**  
Depois que o corpus P0.1 puder provar conclusão prematura e conclusão legítima.

**Por quê**  
O livro volta repetidamente ao mesmo erro: texto convincente, tool call bem-sucedida ou ausência de exceção não confirmam o estado real.

**Versão mínima**

- tarefas explicativas: `not_applicable`;
- tarefas de mudança: exigir pelo menos uma evidência coerente com os critérios aceitos;
- evidência referenciada por sequência de evento, artifact ou estado verificado;
- se não houver verificador, dizer “finalizado pelo provider, não verificado”, sem inventar falha.

**Exemplos de evidência**

- teste ou build relevante;
- read-back do arquivo alterado;
- diff dentro do escopo;
- consulta de estado externo;
- screenshot/render quando o resultado é visual;
- confirmação humana quando o critério é subjetivo.

**Gate de aceitação**  
Bloquear falsa conclusão nos casos negativos sem transformar toda conversa informativa em workflow obrigatório.

### P0.3 — Atribuição da primeira falha

**O quê**  
Um analisador offline que encontre o primeiro evento no qual a trajetória divergiu do conjunto aceitável.

**Onde**  
Sobre `SessionEvent` e JSONL já existentes; inicialmente como comando de benchmark ou script local, não dentro do hot path do runtime.

**Quando**  
Assim que P0.1 gerar as primeiras falhas reproduzíveis.

**Por quê**  
“O modelo falhou” não indica o que corrigir. A origem pode ser contexto ausente, schema, serialização, seleção de tool, matching, tool, estado do ambiente ou conclusão prematura.

**Taxonomia mínima**

- requisito/contexto;
- provider/transporte;
- parsing/protocolo;
- seleção de tool;
- argumentos;
- execução da tool;
- interpretação do resultado;
- violação de processo;
- relato final incorreto;
- cancelamento/efeito desconhecido;
- término prematuro.

**Gate de aceitação**  
Cada diagnóstico aponta `seq`, call ID, evidência e confiança; não apenas uma frase gerada.

## Prioridade P1 — melhorar o contexto e a segurança do loop

### P1.1 — Cápsula de estado para o modelo

**O quê**  
Uma projeção curta e determinística anexada ao final da requisição do provider, sem entrar no system prompt e sem acumular versões antigas.

**Onde**  
No runtime, imediatamente antes da chamada ao provider. Os dados já estão distribuídos entre `AgentLoopConfig`, `TodoTracker`, `ContextBudget`, `CompactionHandle`, cwd e o guard de repetição.

**Quando**  
Depois do baseline P0.1, em A/B local por provider/modelo.

**Por quê**  
A TUI já conhece atividade, TODO e uso de contexto, mas o modelo precisa reconstruir isso do histórico. O capítulo 2 argumenta que modelos recuperam fatos melhor do que contam e agregam estado longo.

**Campos iniciais**

```text
mode
cwd
turn/max_turns
read_budget_remaining
mutation_budget_remaining
current_todo
context_estimate/window
compaction_status
last_repeated_failure
modified_artifact_refs
expected_next_gate
```

**Restrições nativas**

- calculada por código, nunca por outro LLM;
- sem timestamp se o tempo não mudar a decisão;
- sem corpo completo de logs;
- não apagar a trajetória original;
- não persistir cópias repetidas;
- formato e papel validados por adapter;
- feature local desativável para ablação.

**Gate de aceitação**  
Reduzir repetição, esquecimento de TODO ou falsa conclusão sem piorar tokens, cache e tarefas curtas.

### P1.2 — Descoberta explícita de instruções do workspace

**O quê**  
Descobrir, limitar e rotular arquivos de instrução controlados pelo usuário, como `AGENTS.md` e `RULES.md`, com precedência e escopo claros.

**Evidência da lacuna**  
Na busca pelas fontes Rust de `crates`, a única ocorrência de `AGENTS.md`/`RULES.md` encontrada foi o comentário do system prompt em [`provider.rs`](D:/Slim/crates/slim-core/src/provider.rs:317); não foi localizada montagem efetiva desse contexto.

**Onde**  
Uma função pequena em `slim-core::context`, chamada na abertura da sessão e quando o cwd mudar. A CLI fornece o workspace; o runtime recebe um fragmento já limitado e com proveniência.

**Quando**  
Depois da cápsula de estado, pois ambos precisam de um contrato claro de contexto dinâmico.

**Por quê**  
O livro trata instruções do projeto como memória operacional estável. Hoje o prompt diz que esse conhecimento vive em `AGENTS.md/skills`, mas o modelo não aparenta recebê-lo automaticamente.

**Versão mínima**

- nomes explicitamente permitidos;
- busca do root ao cwd;
- limite total de bytes;
- path, hash e precedência visíveis;
- conteúdo rotulado como contexto consultivo do workspace, distinto de arquivo arbitrário;
- precedência sempre abaixo de system/developer/user/Harness: o arquivo não amplia escopo, não concede permissão e não autoriza ação destrutiva ou externa;
- erro explícito para arquivo grande ou ilegível.

**Não fazer**  
Não injetar README inteiro, varrer toda documentação, executar instruções embutidas em arquivos externos ou criar indexador semântico.

### P1.3 — Semântica de cancelamento com efeito desconhecido

**O quê**  
Distinguir:

- cancelado antes de iniciar;
- cancelado sem efeito possível;
- cancelado depois que um efeito pode ter ocorrido;
- concluído.

**Evidência da lacuna**  
O runtime reconhece que uma tool pode já ter produzido efeito e, em seguida, apenas força `success = false`: [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:2416).

**Onde**  
No resultado de tool e no ledger durável. O schema já possui `DurableOutcome::Unknown`, que é a representação adequada: [`schema_v2.rs`](D:/Slim/crates/slim-core/src/session/schema_v2.rs:150).

**Quando**  
Antes de qualquer retry durável, retomada automática, execução externa ou child agent mutante.

**Por quê**  
Repetir uma ação não idempotente após cancelamento ambíguo pode duplicar escrita, envio, pagamento ou publicação.

**Gate de aceitação**  
Uma mutação com efeito desconhecido nunca é repetida automaticamente; o próximo passo deve consultar estado ou pedir decisão.

### P1.4 — Melhorar ACI e resultados longos sem novas tools

**O quê**

1. ampliar descrições das tools atuais;
2. explicitar path efetivo, paginação, truncamento e precondições;
3. mudar saída longa de shell de “somente início” para “início + fim”;
4. quando útil, usar o `ArtifactStore` existente para o conteúdo completo antes do preview.

**Onde**  
Schemas em [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:580), shell em [`tools/mod.rs`](D:/Slim/crates/slim-core/src/tools/mod.rs:560) e materialização já existente em [`runtime/mod.rs`](D:/Slim/crates/slim-core/src/runtime/mod.rs:2495).

**Quando**  
Depois que P0.1 medir seleção errada, diagnóstico perdido no fim da saída ou releituras causadas por truncamento.

**Por quê**  
O capítulo 4 mostra que descrição e fidelidade muitas vezes valem mais que trocar o modelo. Logs de compilação costumam colocar contexto no início e causa final no fim.

**Gate de aceitação**  
Melhorar correção/recuperação sem aumentar o número de tools nem inserir schemas grandes.

### P1.5 — Telemetria local por fase e por cache

**O quê**  
Completar a observabilidade existente com campos diagnósticos, mantendo conteúdo sensível fora das métricas.

**Onde**  
Em `ProviderEvent`, `EventKind`, uso durável e projeção da TUI.

**Quando**  
Junto da avaliação, antes de otimizar latência ou custo.

**Campos úteis**

- tempo de conexão;
- headers;
- primeiro byte;
- primeiro conteúdo semântico;
- duração de tool;
- duração e custo de compactação;
- prompt-cache creation/read, quando o provider expuser;
- hit/miss do cache local, separado do remoto;
- stop reason;
- retries e classificação;
- tamanho de tools e histórico por turno.

**Por quê**  
“Provider lento” pode significar fila, prefill, rede, ausência de primeiro conteúdo, tool ou compactação. Métrica agregada induz correção errada.

**Gate de aceitação**  
Explicar uma regressão de custo/latência sem gravar prompt, código, segredo ou conteúdo de tool em analytics externo.

## Prioridade P2 — memória local governada

### P2.1 — Memória de projeto em duas camadas

**O quê**  
Usar sessões como evidência append-only e manter um panorama pequeno, aprovado e derivado para decisões, preferências e fatos duráveis.

**Onde**  
Sobre session JSONL, `DurableFact`, checkpoints e filesystem local. Não introduzir banco vetorial no primeiro passo.

**Quando**  
Somente depois de:

- schema v2 estar integrado de forma produtiva no fluxo relevante;
- regras de escopo/privacidade estarem definidas;
- o corpus de avaliação medir recall básico e conflito;
- o usuário optar pela memória.

**Por quê**  
O Slim já retoma trajetórias, mas retomar transcript não é o mesmo que recordar uma decisão de projeto com fonte e validade. O capítulo 3 propõe “panorama residente + detalhe recuperado sob demanda”.

**Cartão mínimo**

```text
namespace/key
value
scope: user | project
kind: episodic | semantic | procedural
source_event_refs
observed_at
valid_until
status: candidate | active | conflicting | expired | archived
privacy_class
```

**Recuperação inicial**

- exact match;
- busca textual existente;
- filtros por projeto/data/tipo;
- ranking simples por correspondência e recência;
- leitura da evidência original antes de usar um fato sensível.

**Gates em três níveis**

1. recordar um fato explícito corretamente;
2. combinar duas sessões sem misturar entidades;
3. sugerir algo proativamente apenas com evidência suficiente.

Se o nível 1 não for confiável, o nível 3 fica desligado.

**Não fazer**  
Não memorizar tudo, não injetar logs completos, não sobrescrever conflito, não cruzar projetos, não usar cloud para sanitizar segredos e não instalar embeddings antes de benchmark.

## Prioridade P3 — somente quando a demanda provar necessidade

### P3.1 — Descoberta hierárquica de tools e MCP real

**Quando usar**  
Quando integrações reais fizerem o catálogo crescer a ponto de degradar tokens ou seleção.

**Como manter nativo**

- reutilizar `CapabilityCatalog`, `RuntimeCapabilityBridge` e a estratégia lazy das Skills;
- carregar metadata primeiro e schema completo sob demanda;
- manter ordem estável;
- pin de versão e origem;
- permissão fora da Skill;
- transporte concreto implementado apenas para a integração escolhida.

**Por que não agora**  
Com o catálogo atual ainda pequeno — sete tools-base e poucas tools adicionadas pelo runtime —, busca semântica de tools e todos os schemas MCP seriam custo sem problema comprovado.

### P3.2 — Execução real de subagentes

**Quando usar**

- subtarefas independentes e verificáveis;
- pesquisa ampla que poluiria o contexto principal;
- reviewer com evidência nova;
- paralelismo que compensa custo e latência.

**Contrato mínimo de handoff**

- objetivo;
- limites;
- fatos confirmados;
- artifact paths;
- trabalho restante;
- orçamento;
- critério de término;
- ownership de arquivos.

**Como manter nativo**  
Reutilizar scheduler, cancellation token, mutation lease, capability ledger, JSONL e artifact paths. Workers read-only por padrão; escrita concorrente apenas isolada e com ownership explícito.

**Por que não agora**  
O capítulo 10 é explícito: um segundo Agent só ajuda quando traz informação nova. Mesma entrada + mesmo modelo + mesmo contexto costuma multiplicar custo e convergir ao mesmo erro.

### P3.3 — Proposer–Reviewer seletivo

**Quando usar**

- release;
- operação irreversível;
- UI/PDF/render visual;
- segurança;
- migração de dados;
- artefato em que há evidência independente.

**Regras**

- reviewer recebe o candidato atual e evidência, não toda a justificativa do proposer;
- reviewer não pode editar testes, gates ou coletor de evidência;
- veto deve apontar condição reparável;
- limite de iterações;
- mesmo Agent “revendo a si próprio” não conta como independência.

### P3.4 — Evolução contínua offline

**O quê**  
Um ciclo acionado explicitamente que agrupa falhas recorrentes, propõe a menor alteração e a testa contra boundary e retention sets.

**Quando**  
Somente depois de P0/P1 produzirem avaliações confiáveis e volume real de trajetórias.

**Roteamento da correção**

- fato contextual → memória/knowledge;
- procedimento explicável → Skill/instrução;
- regra determinística → código/Harness;
- capacidade implícita do modelo → trocar modelo ou, fora do Slim, post-training.

**Raiz imutável**

O ciclo não pode alterar seus próprios:

- verificadores;
- testes;
- thresholds;
- logs de auditoria;
- backups;
- permissões;
- mecanismo de promoção.

**Por que não começar por self-modification**  
Registrar, comparar e validar uma classe de falha é o primeiro sucesso. “O Slim se autoedita” antes disso apenas automatiza conjecturas.

## Ideias do livro que o Slim deve preservar como estão

1. system prompt curto, processual e estável;
2. mensagens nativas do provider, sem achatar roles em texto;
3. tools gerais e em pequeno número;
4. schemas determinísticos e argumentos JSON;
5. read-only paralelo, mutação serial;
6. precondições em write/patch;
7. paginação e limites em read/list/search;
8. proteção contra repetição da mesma falha;
9. deduplicação de outputs idênticos;
10. compactação em lote, com instrução raiz e grupos tool preservados;
11. Skills sob demanda, com trust e contenção;
12. interação `ask_question` tipada e limitada;
13. cancelamento e progresso estruturados;
14. event stream e TUI como projeção, não como autoridade de provider;
15. sessões e artefatos como base de replay;
16. seams de MCP/subagentes sem obrigar a implementação prematura.

## Ideias que não devem ser adotadas agora

### Infraestrutura sem demanda

- swarm, society of agents ou debate por padrão;
- message bus, Redis, RabbitMQ ou A2A dentro do Slim;
- MCP completo antes de existir integração alvo;
- marketplace remoto de Skills;
- VM, Docker, microVM ou WSL por Agent;
- terminal persistente novo sem caso stateful demonstrado;
- sistema geral de plugins apenas porque o livro descreve composição dinâmica.

### Recuperação e memória excessivas

- vector database como dependência inicial;
- GraphRAG, RAPTOR ou reranker para sessões pequenas;
- Agentic RAG em toda pergunta;
- memória automática de toda trajetória;
- reescrita destrutiva de fatos conflitantes;
- injeção de toda documentação no prompt;
- LLM adicional para resumir estado que o código já conhece.

### Segurança aparente

- blacklist textual como defesa principal de shell;
- Sidecar LLM para toda chamada local;
- parâmetros autorrelatados pelo modelo como prova de autorização;
- Skill como limite de permissão;
- retry cego após timeout/cancelamento;
- placeholder tratado como tool concluída;
- modelo aprovando a própria conclusão sem evidência.

### Treinamento fora da autoridade do produto

- pre-training, mid-training, SFT, PPO, GRPO, LoRA ou reward model dentro do Slim;
- coletar CoT de providers fechados;
- RL sobre APIs reais com efeitos colaterais;
- simulador textual tratado como produção;
- destilação antes de o comportamento estar estável;
- roteamento multi-modelo antes de workload e avaliação demonstrarem ganho.

### Modalidades fora do produto atual

- voz, ASR, TTS ou full-duplex;
- Computer Use visual e coordenadas;
- mobile automation;
- robótica, VLA ou world models.

Os princípios transferíveis dessas áreas — safe points, observação após ação, ação delimitada e confirmação do estado — já aparecem nas prioridades anteriores.

## Sequência recomendada

### Etapa 0 — baseline de avaliação

**Objetivo:** congelar os casos, critérios e métricas antes de tocar o Harness.  
**Saída:** 8–12 casos locais, relatório reproduzível e configuração registrada.  
**Não inclui:** mudança de prompt, provider ou runtime.

### Etapa 1 — avaliação e evidência

**Objetivo:** distinguir provider completion, outcome e process veto.  
**Saída:** resultado da tarefa, primeira falha e evidências referenciadas.  
**Gate:** detecta conclusão prematura sem bloquear tarefa legítima.

### Etapa 2 — contexto operacional

**Objetivo:** testar cápsula de estado e descoberta de instruções do workspace.  
**Saída:** A/B local com tokens, sucesso, chamadas redundantes e compactação.  
**Gate:** ganho real sem degradação no retention set.

### Etapa 3 — efeitos e observabilidade

**Objetivo:** estados de cancelamento, ACI mais claro, head/tail e métricas de cache/fase.  
**Saída:** diagnóstico causal melhor e retry seguro.  
**Gate:** nenhuma mutação ambígua repetida automaticamente.

### Etapa 4 — memória opt-in

**Objetivo:** recall local com proveniência e conflito.  
**Saída:** panorama pequeno + busca da evidência original.  
**Gate:** nível 1 e 2 confiáveis antes de proatividade.

### Etapa 5 — capacidades condicionais

**Objetivo:** MCP, subagentes ou evolução offline somente para casos medidos.  
**Saída:** uma integração ou uma classe de falha, não uma plataforma genérica.  
**Gate:** ganho superior ao custo, latência e complexidade.

## Critérios para dizer que o Slim melhorou

Uma mudança futura não deve ser aceita porque “parece mais inteligente”. Ela precisa mostrar:

- aumento de `Pass@1` no boundary set;
- aumento de confiabilidade em repetições quando o caso é crítico;
- nenhuma violação de processo;
- nenhuma degradação material no retention set;
- menor ou igual número de calls redundantes;
- custo e latência dentro do orçamento;
- causa de falha mais diagnosticável;
- nenhuma expansão não autorizada de dados, rede ou permissões;
- rollback simples.

Para mudanças de baixo volume, o relatório deve mostrar amostra e incerteza. Um ganho de poucos pontos em poucos casos não justifica arquitetura nova.

## Conclusão

O melhor do livro não é uma feature isolada. É uma disciplina: **não corrigir um agente por intuição quando o Harness pode observar, limitar, verificar e aprender com evidência**.

O Slim já tem uma base tecnicamente compatível com essa disciplina. Seu próximo salto nativo deveria ser:

> **transformar a trajetória atual em resultado verificável e diagnóstico reproduzível, antes de ampliar a superfície de ação.**

Em termos práticos, a ordem é:

1. avaliação local;
2. conclusão verificada;
3. primeira falha;
4. estado operacional para o modelo;
5. instruções do workspace;
6. cancelamento e observabilidade refinados;
7. memória opt-in;
8. MCP/subagentes/evolução somente quando os dados exigirem.

Isso preserva a natureza do Slim: local-first, pequeno, legível, reversível e sem engenharia excessiva.

## Limitações desta análise

- Os experimentos e números do livro foram estudados, não reproduzidos.
- A comparação usa o checkout local de 29 de agosto de 2026; áreas em WIP podem mudar.
- Buscas de ausência foram limitadas às fontes, testes e benchmarks explicitamente inspecionados.
- Não foi executado provider, teste, build ou benchmark, porque o produto não foi alterado.
- O relatório recomenda caminhos; não congela arquitetura nem autoriza implementação.
