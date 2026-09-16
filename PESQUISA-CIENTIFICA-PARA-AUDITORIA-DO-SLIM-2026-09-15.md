# Pesquisa científica recente para avaliar melhorias no Slim

**Data do levantamento:** 15 de setembro de 2026  
**Projeto-alvo:** Slim (`D:\\Slim`)  
**Finalidade:** documento de referência para um agente técnico analisar, no código atual do Slim, se técnicas recentes de pesquisa justificam protótipos ou mudanças no harness.

## Objetivo da análise

Avalie o Slim à luz dos trabalhos abaixo. O objetivo não é implementar papers por analogia nem presumir que os ganhos publicados se transferem ao projeto. É localizar, no código e em medições do Slim, oportunidades concretas de:

- reduzir input tokens sem perder contexto necessário;
- melhorar seleção e execução de ferramentas;
- reduzir chamadas redundantes, retries improdutivos e repetição após erros;
- aumentar taxa de conclusão real e verificável;
- preservar prefix caching quando economicamente vantajoso;
- melhorar o harness de forma independente de provider sempre que possível.

Nesta etapa, faça **somente análise**. Não implemente alterações sem aprovação.

## Regras para o agente que receber este documento

1. Leia primeiro o `AGENTS.md` e as instruções aplicáveis do repositório.
2. Preserve todas as alterações preexistentes; considere que o checkout pode estar sujo.
3. Não reinicie processos, não interrompa agentes, não altere configurações reais, não execute chamadas pagas e não exponha prompts privados, mensagens, credenciais ou dados sensíveis.
4. Baseie as conclusões no código atual, não apenas em documentação ou relatórios antigos.
5. Diferencie rigorosamente:
   - **fato observado no Slim**;
   - **resultado reportado pelo paper**;
   - **inferência de aplicabilidade**;
   - **hipótese que ainda precisa de experimento**.
6. Não recomende uma técnica apenas porque ela apresentou ganho em outro workload. Exija um caminho alcançável no Slim, benefício plausível, risco identificado e teste A/B capaz de refutar a proposta.
7. Não trate preprint como evidência equivalente a paper revisado por pares.
8. Para cada proposta aplicável, informe arquivos, símbolos e fluxo atingidos; mudança mínima; instrumentação necessária; métricas; riscos; critério de aprovação e rollback.
9. Caso o Slim já implemente a ideia de forma correta, documente a evidência e não proponha uma segunda camada redundante.

## Síntese executiva

A literatura selecionada converge em quatro princípios com alta relevância potencial para o Slim:

1. **Contexto deve ser governado, não apenas acumulado.** Histórico completo pode aumentar custo e também piorar conclusão. A unidade segura de compactação tende a ser semântica — turno, tool span, artefato, decisão, evidência — e não uma quantidade fixa de tokens.
2. **Ferramentas devem ser recuperadas antes de serem escolhidas.** Expor catálogos grandes ou redundantes ao modelo aumenta tokens e ambiguidade. Shortlists contextuais podem melhorar seleção, desde que o recall e a segurança sejam medidos.
3. **Erro de ferramenta é um estado de controle próprio.** Após falha, repetir o loop normal tende a gerar chamadas redundantes. O harness pode classificar e reparar deterministicamente os casos inequívocos e reservar reflexão curta para os ambíguos.
4. **Conclusão deve depender de estado e evidência.** A mensagem “terminei” não comprova que arquivos, comandos, testes e objetivos chegaram ao estado esperado.

Cache-aware compression forma um quinto eixo, mas é parcialmente dependente de provider e de seu modelo de cobrança. ReCache é ainda mais específico: depende de controle do serving e do KV cache, portanto não deve orientar o harness agnóstico do Slim salvo no backend local.

## Trabalhos prioritários

### 1. Referência: [Self-GC: Self-Governing Context for Long-Horizon LLM Agents](https://arxiv.org/abs/2607.00692)

**Status da evidência:** preprint do arXiv, submetido em 1º de julho de 2026; não equivale a peer review. É, porém, o trabalho mais diretamente alinhado ao problema de lifecycle de contexto em agentes long-horizon.

**Mecanismo estudado:** o histórico deixa de ser tratado como um buffer linear. Turnos de usuário, spans de ferramentas e estado de skills tornam-se objetos indexados. Um planner lateral propõe ações `fold`, `mask` e `prune`; o harness valida as ações, preserva sidecars recuperáveis e só efetiva mudanças em limites seguros e considerando o custo de invalidar cache.

**Resultados reportados pelos autores:**

- Hard Set com 33 sessões: remoção de **43,95% dos tokens de prefixo**, com **84,85%** das continuações futuras classificadas como sem impacto; baselines heurísticos ficaram entre **54,55% e 69,70%** de no-impact rate.
- Suíte derivada de produção com 332 sessões: três planners atingiram **91,27%–94,58%** de no-impact rate; baselines, **77,71%–87,46%**.
- Split online por conta em produção: **10%–15% menos input tokens** em média durante o dia, com picos próximos de 20%.

**Tradução plausível para o Slim:**

- representar separadamente instruções imutáveis, mensagens do usuário, resultados de ferramentas, referências a arquivos, erros transitórios, decisões e evidências;
- atribuir identidade estável e dependências a unidades de contexto;
- permitir compactação reversível de outputs volumosos, preservando locator, hash, status, resumo mínimo e forma de recuperar o conteúdo integral;
- impedir GC no turno ativo, na última intenção do usuário, em chamadas pendentes e em objetos referenciados por planos, arquivos ou validações;
- contabilizar custo do planner e cache miss antes do commit.

**Riscos:** planner lateral gera chamada adicional; avaliação de “sem impacto” pode não capturar dependências raras; sidecars introduzem lifecycle, concorrência, privacidade e durabilidade; edição no meio do prefixo pode destruir cache mais valioso do que os tokens removidos.

**Experimento mínimo no Slim:** primeiro instrumentar, sem mudar comportamento, quais objetos do contexto são reutilizados por turnos futuros. Depois comparar baseline contra uma política conservadora que apenas dobre outputs de ferramentas já materializados em arquivos ou substituídos por resultados mais novos. Medir sucesso, reaberturas do sidecar, tokens líquidos incluindo o planner, cache hit, latência, chamadas redundantes e falhas de retomada.

**Prioridade sugerida:** **P0 para investigação**, não para implementação imediata.

---

### 2. Referência: [Less Context, Better Agents: Efficient Context Engineering for Long-Horizon Tool-Using LLM Agents](https://arxiv.org/abs/2606.10209)

**Status da evidência:** preprint do arXiv de 8 de junho de 2026, aproximadamente uma semana fora do recorte estrito de 90 dias adotado neste levantamento. Não revisado por pares. O próprio paper limita a generalização a uma classe de workflow empresarial.

**Mecanismo estudado:** retenção apenas dos últimos cinco pares tool call/response, opcionalmente acompanhada de resumo automatizado dos pares removidos. O teste usa GPT-5, MCP e itemização de despesas de hotel no Dynamics 365.

**Resultados reportados:** médias de cinco execuções independentes sobre 50 tarefas:

- histórico completo: **71,0%** de conclusão, **1.480.996 tokens**, **14,56 h**;
- últimos cinco pares de ferramentas: **79,0%**, **535.274 tokens**, **5,39 h**;
- últimos cinco pares + resumo: **91,6%**, **553.374 tokens**, **5,79 h**.

Portanto, a configuração com resumo usou cerca de **62,6% menos tokens** que o histórico completo e obteve **+20,6 pontos percentuais** de conclusão naquele workload.

**Tradução plausível para o Slim:** não copiar o número cinco. Investigar uma janela recente formada por unidades semânticas completas e um snapshot estruturado de estado antigo: objetivo atual, restrições do usuário, decisões, arquivos modificados, erros não resolvidos, testes executados e próximos passos.

**Riscos:** o workload é estreito; resultados verbosos do ERP acentuam artificialmente o benefício; resumo pode perder paths, IDs, valores exatos, comandos, resultados de testes e preferências negativas; uma janela fixa pode quebrar debugging ou retomada longa.

**Experimento mínimo:** reproduzir tarefas reais e longas do Slim com três políticas: histórico atual; janela semântica recente; janela + snapshot estruturado. Avaliar com o mesmo modelo, seed/configuração quando disponível e repetição suficiente para intervalos de confiança.

**Prioridade sugerida:** **P0 para benchmark e desenho de política conservadora**.

---

### 3. Referência: [ToolScope: Enhancing LLM Agent Tool Use through Tool Merging and Context-Aware Filtering](https://aclanthology.org/2026.acl-long.1573/)

**Status da evidência:** paper revisado por pares, ACL 2026 Long Papers, publicado em julho de 2026. A primeira versão pública no arXiv é anterior; o status recente relevante é a publicação formal na ACL.

**Mecanismo estudado:** duas camadas complementares: auditoria/merge de ferramentas redundantes com autocorreção e recuperação contextual das ferramentas mais relevantes antes da seleção final pelo LLM.

**Resultado reportado:** ganhos de **8,38% a 38,6% na precisão de seleção de ferramentas**, avaliados em três modelos e três benchmarks open source.

**Tradução plausível para o Slim:**

1. inventariar schemas efetivamente expostos por turno;
2. identificar sobreposição de nome, descrição, propósito e argumentos;
3. agrupar ou desambiguar ferramentas redundantes sem alterar contratos públicos de forma insegura;
4. recuperar um conjunto candidato por intenção, capacidade, workspace e estado;
5. disponibilizar fallback seguro para o catálogo completo quando confiança/recall forem insuficientes;
6. validar argumentos contra schema e política antes de executar.

**Riscos:** o retriever pode excluir justamente a ferramenta rara necessária; merge pode esconder diferenças de permissão ou side effects; descrições de ferramentas externas podem conter prompt injection; shortlist muda o prefixo e o comportamento de cache; uma camada sem telemetria pode apenas deslocar erros.

**Experimento mínimo:** replay offline de traces do Slim com ground truth derivado da ferramenta realmente necessária, medindo Top-k recall, precisão, tokens de schema, taxa de fallback, chamadas incorretas e sucesso da tarefa. O gate primário deve ser recall alto; economia de tokens é secundária à disponibilidade da ferramenta correta.

**Prioridade sugerida:** **P0 se o Slim injeta um catálogo amplo ou variável; P1 se o catálogo normal já é pequeno**.

---

### 4. Referência: [Scalable LLM Agent Tool Access in the Cloud](https://arxiv.org/abs/2607.15593)

**Status da evidência:** preprint de 17 de julho de 2026; inclui experiência declarada de deployment, mas não deve ser tratado como paper revisado por pares.

**Mecanismo estudado:** gateway MCP em escala de nuvem com adaptação de serviços legados, compatibilidade entre variantes, controle de acesso, recomendação híbrida de ferramentas e roteamento com afinidade de sessão.

**Resultados reportados:** **98% de Top-15 recall**, acesso a mais de **3.000 ferramentas**, seleção **8,9× mais rápida** e **23,8× menos tokens** na seleção, com baixa sobrecarga por chamada e estabilidade sob scale-out.

**Tradução plausível para o Slim:** reforça ToolScope em escala maior: separar descoberta de capacidades, shortlist e seleção do modelo. Também sugere que metadados de sessão, ACL e compatibilidade pertencem ao gateway/registry, não ao prompt.

**Limite de aplicabilidade:** os ganhos absolutos com 3.000 ferramentas não podem ser projetados para um catálogo pequeno. Se o Slim normalmente expõe poucas tools, a complexidade de um retriever pode não se pagar.

**Experimento mínimo:** medir distribuição real do número e tamanho dos schemas por request, percentual do input ocupado por ferramentas e confusões entre tools semelhantes. Só prototipar retrieval se esses dados mostrarem pressão material.

**Prioridade sugerida:** **P1, condicionada à escala real do catálogo**.

---

### 5. Referência: [Failure Makes the Agent Stronger: Enhancing Accuracy through Structured Reflection for Reliable Tool Interactions](https://aclanthology.org/2026.findings-acl.618/)

**Status da evidência:** revisado por pares, Findings of ACL 2026, publicado em julho. A versão de preprint surgiu em 2025; o aceite formal é recente.

**Mecanismo estudado:** reflexão estruturada e treinável na sequência `Erroneous Call → Reflection → Corrected Call → Final`. A reflexão deve diagnosticar a falha usando evidência do passo anterior e produzir uma chamada seguinte executável. O trabalho avalia BFCL v3 e o Tool-Reflection-Bench.

**Resultado reportado:** melhorias significativas em sucesso multi-turn e recuperação de erros, com redução de chamadas redundantes. O resumo oficial não fornece um único percentual agregado; portanto, não se deve inventar um número geral.

**Tradução plausível para o Slim:** implementar conceitualmente um estado de recovery, mas sem reflexão paga em toda chamada:

1. classificar erro por parsing, schema, argumento, path, permissão, timeout, cancelamento, processo encerrado ou falha do domínio;
2. corrigir deterministicamente apenas casos inequívocos e seguros;
3. para os demais, fornecer ao modelo um `repair_context` mínimo: chamada anterior, schema relevante, erro sanitizado, estado atual e restrições;
4. bloquear repetição idêntica sem nova evidência;
5. aplicar budget de tentativas e escalada clara ao usuário.

**Riscos:** reflexão adicional pode aumentar custo e latência; erro sanitizado em excesso elimina a causa; correção automática de argumentos pode alterar intenção ou permissão; bloquear repetições pode impedir retry válido de falhas transitórias.

**Experimento mínimo:** conjunto reproduzível de falhas reais por classe, comparando loop atual, repair determinístico e repair assistido. Medir recovery rate, chamadas redundantes, tokens até recuperação, tempo e alterações indevidas de intenção.

**Prioridade sugerida:** **P0 para análise do caminho de erro e retry**.

---

### 6. Referência: [Agentic Context Strategies for Multi-Format Document Understanding: When Should Language Models Use Tools?](https://aclanthology.org/2026.acl-industry.133/)

**Status da evidência:** revisado por pares, ACL 2026 Industry Track, julho de 2026.

**Mecanismo estudado:** comparação de quatro estratégias de contexto, seis modelos frontier e documentos Word, Excel e PowerPoint. Contrasta contexto/RAG passivos com exploração ativa apoiada por ferramentas.

**Resultados reportados:** `RAG + Tools` obteve **46% de acurácia**, contra **6%** de `RAG-only`; benefícios de ferramentas foram consistentes em formatos e modelos (**+28 a +40 pontos**). Routing inteligente teve mais impacto que aumentar o número de iterações.

**Tradução plausível para o Slim:** para workspaces e documentos grandes, o modelo deve poder buscar conteúdo sob demanda por ferramentas de navegação e code intelligence, em vez de receber dumps preventivos. O roteador deve escolher a fonte e a ferramenta adequadas, não simplesmente liberar mais loops.

**Limite de aplicabilidade:** o estudo é de document QA multimodal, não de coding agent. Ele apoia a tese de recuperação ativa, mas não prova ganhos diretos em edição de código.

**Experimento mínimo:** comparar tarefas do Slim com contexto pré-carregado versus busca ativa no workspace, mantendo objetivo e modelo. Medir sucesso, arquivos lidos, bytes/tokens transferidos, tool calls, tempo até a primeira evidência útil e omissões críticas.

**Prioridade sugerida:** **P1, especialmente para repositórios grandes e artefatos não textuais**.

---

### 7. Referência: [Coding-agents Can Replicate Scientific Machine Learning Papers](https://arxiv.org/abs/2607.02134)

**Status da evidência:** preprint de 2 de julho de 2026; não revisado por pares. Avaliação pequena — 12 execuções em quatro papers —, mas diretamente relacionada a coding agents e conclusão baseada em evidência.

**Mecanismo estudado:** workflow Paper-replication com claims transformados em targets persistentes, experimentos executados, provenance, ligação entre outputs e claims, cobertura no relatório e validation checks antes de concluir.

**Resultados reportados:** **12/12 workspaces** passaram pelo completion gate e **158/158 targets** registrados tiveram cobertura no relatório. Isso não significa fidelidade científica perfeita; os autores relatam variação entre execuções em decomposição, fidelidade numérica, tempo e critérios de aceitação.

**Tradução plausível para o Slim:** manter um ledger leve de objetivo → evidência → verificação → status para tarefas mutantes. Exemplos de evidência: diff, arquivo realmente criado, exit code, teste executado, diagnóstico rechecado e artefato renderizado.

**Riscos:** transformar toda tarefa em checklist rígido gera burocracia e tokens; evidência disponível pode estar errada ou incompleta; passar em testes não garante ausência de regressão; goals mal decompostos produzem falsa cobertura.

**Experimento mínimo:** ativar completion gate apenas em tarefas de mudança com critérios objetivos. Comparar taxa de falso “concluído”, reabertura pelo usuário, falhas não detectadas, tokens e duração.

**Prioridade sugerida:** **P0 para tarefas de código com mutação; dispensável em respostas simples**.

---

### 8. Referência: [Toward Scalable Verifiable Reward: Proxy State-Based Evaluation for Multi-turn Tool-Calling LLM Agents](https://aclanthology.org/2026.acl-industry.87/)

**Status da evidência:** revisado por pares, ACL 2026 Industry Track, julho de 2026.

**Mecanismo estudado:** cenários especificam objetivo, fatos do usuário/sistema, estado final esperado e comportamento esperado. Um state tracker infere estado proxy a partir do trace; judges verificam conclusão e hallucinations de tool/user contra as restrições.

**Resultados reportados:** rankings estáveis e diferenciadores entre modelos/esforços de reasoning; transfer de supervisão para cenários não vistos; hallucination do simulador próxima de zero sob especificação cuidadosa; concordância humano–LLM **superior a 90%**.

**Tradução plausível para o Slim:** construir avaliação automatizada de regressão em traces multi-turn sem depender exclusivamente de string matching. Sempre que existir backend determinístico — filesystem, SQLite, git diff, exit code, processo, JSON de ferramenta — ele deve continuar sendo a fonte primária; state proxy/judge serve para o que não puder ser verificado deterministicamente.

**Riscos:** judge e agente podem compartilhar vieses; estado proxy pode omitir side effects; vazamento entre cenário, resposta e critério infla resultados; >90% de concordância não é 100% e pode esconder falhas graves raras.

**Experimento mínimo:** criar um corpus versionado de tarefas do Slim com invariantes determinísticos e rubric semântica separada. Calibrar o judge contra revisão humana antes de usá-lo como gate.

**Prioridade sugerida:** **P0 como infraestrutura de P&D**, pois as demais propostas não devem ser aceitas sem avaliação confiável.

---

### 9. Referência: [Cache-Aware Prompt Compression: A Two-Tier Cost Model for LLM API Caching](https://arxiv.org/abs/2607.15516)

**Status da evidência:** preprint de 17 de julho de 2026; não revisado por pares. Parte da caracterização é específica à API e ao modelo avaliados.

**Mecanismo estudado:** compressão query-agnostic do prefixo estável, `cache_control` explícito e limite de compressão que evita deslocar o prefixo para uma faixa de cache economicamente pior. O paper mostra que compressão query-aware pode invalidar cache a cada consulta e ter ROI negativo.

**Resultados reportados:** CAPC foi a estratégia mais barata em **16/16 configurações** no LongBench-v2, com economia média de **49% contra cache-only**, **64% contra query-aware compression** e **90% contra vanilla**, mantendo qualidade a até 0,05 do baseline. Em tau-bench retail, manteve o mesmo reward de vanilla (**36/50**) e foi a opção mais barata; query-aware compression ficou **40,1% mais cara** que vanilla naquele cenário.

**Tradução plausível para o Slim:** ordenar o prompt em camadas: prefixo estável e cacheável; instruções/capabilities relativamente estáveis; estado de sessão; turno e outputs dinâmicos. Evitar timestamps, IDs variáveis, ordenação não determinística ou resumos query-aware dentro do prefixo estável.

**Riscos:** preços, TTLs, thresholds e semântica de cache variam por provider e modelo; otimizar custo pode piorar latência ou qualidade; tentar padronizar todos os providers por uma observação da Anthropic é arquitetura incorreta.

**Experimento mínimo:** telemetria por provider de tokens cache-read/cache-write/uncached e custo efetivo; replay A/B mantendo o conteúdo semanticamente equivalente; nenhuma política global sem feature detection da capacidade real.

**Prioridade sugerida:** **P1**, abaixo das quatro linhas agnósticas de provider.

---

### 10. Referência: [ReCache: Efficient KV Cache Reuse and Compression for Tool-Augmented LLM Agents](https://arxiv.org/abs/2608.19662)

**Status da evidência:** preprint de 20 de agosto de 2026; não revisado por pares.

**Mecanismo estudado:** cache independente de representações de tools/skills, atenção separada por recurso, posições locais, seleção de rotas layer–KV-head-group e pruning estrutural/semântico de campos.

**Resultados reportados:** desempenho de invocação praticamente igual ao dense baseline (**82,3% vs 82,4% Inv-F1**), **3,655×** de speedup em time-to-first-token, **92,43%** menos memória alocada para tensores KV e atenção **1,423×** mais rápida.

**Tradução plausível para o Slim:** somente se o Slim controlar um backend de inferência local ou serving capaz de alterar atenção, posicionamento e KV cache. Para APIs remotas comuns, isso não é uma otimização implementável no harness.

**Risco arquitetural:** tentar simular ReCache por manipulação textual de prompt não reproduz o método do paper e pode piorar qualidade/cache.

**Experimento mínimo:** nenhum no harness agnóstico. Registrar como trilha específica do backend local; só avaliar com serving compatível e benchmark de tool invocation.

**Prioridade sugerida:** **P2/condicional**.

## Ordem recomendada de investigação no Slim

### Fase 1 — estabelecer baseline confiável

Antes de mudar comportamento, reconstruir e medir:

- montagem integral de cada request;
- bytes/tokens por camada: sistema, histórico, tools, skills, memória, arquivos e resultados;
- chamadas adicionais de resumo, memória, reflexão, título, subagente e background;
- cache read/write/miss quando o provider reportar;
- retries, cancelamentos que continuam consumindo e duplicações;
- seleção da tool correta, erro de argumento e falha de execução;
- conclusão real versus autodeclaração;
- taxa de sucesso por tarefa, turnos, wall time e custo.

Sem esse baseline, qualquer economia será apenas uma hipótese.

### Fase 2 — quatro protótipos isolados, não combinados inicialmente

1. **Janela semântica + snapshot estruturado** — derivada de Less Context, sem copiar a constante cinco.
2. **Shortlist de ferramentas com fallback** — derivada de ToolScope, condicionada ao recall observado.
3. **Recovery específico por classe de erro** — repair determinístico primeiro, reflexão curta apenas quando necessária.
4. **Completion gate baseado em evidência** — ativado somente para tarefas mutantes verificáveis.

Cada protótipo deve ser comparado isoladamente ao baseline. Combinar tudo de início impede atribuir ganhos e regressões.

### Fase 3 — lifecycle avançado e economia de cache

- objetos de contexto indexados e recuperáveis;
- fold/mask/prune com dependências e safe commit;
- política cache-aware por provider;
- exploração ativa de workspace/documentos;
- backend-local KV optimization, se aplicável.

## Matriz de métricas obrigatórias

| Dimensão | Métricas mínimas |
|---|---|
| Qualidade | conclusão real, correção funcional, regressões, restrições preservadas |
| Ferramentas | Top-k recall, seleção correta, argumento válido, falha, redundância, fallback |
| Contexto | input/output tokens, pico de contexto, tokens por camada, informação restaurada |
| Custo | custo total por tarefa, chamadas auxiliares, cache read/write/uncached |
| Eficiência | turnos, tool calls, retries, wall time, TTFT quando disponível |
| Resiliência | recuperação após erro, cancelamento efetivo, retomada, perda de estado |
| Conclusão | falso positivo de “concluído”, targets cobertos, evidência verificável |

## Critério de aceitação de uma proposta

Uma proposta só deve entrar no roadmap de implementação se o agente demonstrar:

1. problema material e alcançável no Slim atual;
2. evidência em trace, teste ou leitura inequívoca do fluxo;
3. técnica compatível com a arquitetura, providers e contratos existentes;
4. experimento A/B reproduzível;
5. melhoria estatística ou operacional relevante em qualidade/custo;
6. nenhuma regressão inaceitável em segurança, contexto, tool recall, cancelamento, retomada ou confiabilidade;
7. mudança mínima e rollback claro.

## Formato esperado do relatório do agente

Entregue uma lista curta e ordenada por impacto. Para cada item:

1. **Veredito:** aplicável agora / precisa de medição / já existe / não aplicável.
2. **Referência científica:** link e mecanismo relevante.
3. **Evidência no Slim:** arquivos, símbolos, fluxo e comportamento observado.
4. **Problema atual:** desperdício ou falha quantificada sempre que possível.
5. **Proposta mínima:** sem abstração hipotética nem reescrita ampla sem necessidade.
6. **Experimento:** baseline, variante, corpus, repetições e métricas.
7. **Riscos e rollback.**
8. **Confiança:** alta, média ou baixa, justificando a classificação.

Se nenhuma técnica tiver aplicação material comprovável no Slim, declare isso. Não force achados para preencher a lista.

## Conclusão técnica

As propostas mais fortes para o Slim não são “usar mais reasoning” nem “aumentar contexto”. São mudanças no controle do runtime: selecionar o que entra no prompt, preservar o que ainda tem dependências, restringir tools candidatas sem perder recall, tratar falhas como estados explícitos e verificar conclusão por evidência externa ao texto do modelo.

O conjunto oferece base suficiente para uma auditoria direcionada, mas ainda não autoriza implementação. Os maiores ganhos publicados ocorreram em workloads específicos; portanto, o resultado correto da próxima etapa deve ser um mapa **paper → hipótese → caminho no Slim → benchmark → decisão**, e não uma lista de features copiadas da literatura.

