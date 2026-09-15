# Harness Slim — etapa 3: contexto e histórico

Data: 2026-09-04. Checkout: `D:\Slim`. Estado: etapa concluída, com dependências de protocolo delimitadas abaixo. Fonte de verdade: código atual; as etapas [1](HARNESS-SLIM-ETAPA-1.md) e [2](HARNESS-SLIM-ETAPA-2.md) foram consultadas como histórico. O diretório existente é `analysis_outputs`.

## Objetivo, aceitação e limites

Fornecer contexto suficiente e fiel, evitando desperdício sem trocar continuidade por uma contagem menor de tokens. Aceitação: correções demonstráveis nas fixtures locais, regressões e gates do projeto, recuperação explícita do texto removido e delimitação do estado específico de protocolo. Sem chamadas pagas, nova dependência, sistema de memória, classificador, múltiplos resumos, transplante do Pit, mudança visual ou migração de sessão.

O checkout de entrada já continha as etapas anteriores e trabalho de OAuth/xAI/TUI/tools. Não houve reset, stash, checkout, branch, commit ou limpeza. Patch inicial e cópias dos fontes ficaram em `%TEMP%\slim-harness-etapa3-20260904`. A comparação de autoria usa essas cópias, não apenas HEAD.

Alterações desta etapa: `crates/slim-core/src/context/{compact,mod}.rs`, `src/provider.rs`, `src/provider/codex.rs`, `src/runtime/mod.rs`, `tests/compaction.rs` e `tests/agent_loop.rs`. Comparação com a entrada: **392 linhas acrescentadas e 127 removidas em sete arquivos**. Uma nova função de teste; demais cenários ampliam fixtures existentes. Nenhum outro fonte Rust foi alterado em relação às cópias de entrada.

## Mapa do fluxo verificado

| Componente | O que entra, se repete ou é descartado | Recuperação e decisão |
|---|---|---|
| Prompt principal | `ProviderConfig::effective_system_prompt`: núcleo nativo constante ou override explícito. Os adapters o serializam em system/instructions a cada request. | Mantido: regras gerais separadas dos schemas. Não reduzir esse prefixo a cada turno nem inserir resumos dentro dele. |
| Instruções do projeto | O caminho CLI examinado não carrega automaticamente AGENTS.md/CLAUDE.md/RULES.md para o system prompt. Arquivos entram quando lidos por ferramentas ou fornecidos pelo usuário/host. | Não confundir as instruções desta execução do Codex com o contexto recebido pelo produto Slim. Nenhum carregador hierárquico foi criado. Instruções system/developer já fornecidas ao core passam a sobreviver à compactação sem virar apenas resumo. |
| Skills | `discovery.rs` lê metadados; `skill list` mostra nomes/descrições sob demanda; o corpo só entra após invocação. Descoberta é memoizada por cwd dentro do run. | Mantido. A slash skill da TUI entra como prefixo do usuário durante a execução e o prefixo literal é retirado do histórico devolvido ao host. Isso não garante remover qualquer paráfrase que o modelo coloque num checkpoint. |
| Schemas | `provider_tool_definitions` deriva a lista do registry/modo e capacidades disponíveis; pergunta depende da rota de interação. A lista é construída antes do loop. | Mantida estável dentro do run. Sem seleção por prompt, descoberta remota adicional ou novos classificadores. |
| Mensagens | Texto, anexos, IDs, nomes e argumentos transitam em `ProviderMessage`. TUI comum conserva `runtime.conversation()`; não reconstrói contexto a partir do texto renderizado. | Preservados argumentos completos de write/patch. Reasoning visível continua sendo evento de apresentação; não é substituto de estado criptografado. |
| Resultados | Limite padrão de 16 KiB no contexto; shell conserva início/fim; demais resultados têm preview. Conteúdo grande é materializado em artefato, com path no resultado. Dedup da etapa 2 só usa marcador menor e evidência ainda ativa. | Mantidos paginação, redaction, conteúdo completo no artefato e invalidação após compactação. A resposta de uma tool já paginada não equivale ao arquivo inteiro original. |
| Estimativa | Pré-cálculo do envelope e depois tamanho serializado real; estimador adaptativo por provider/modelo aprende apenas com usage completo e elegível. Reserva de saída participa do orçamento. | Não é tokenizer. Agora inclui itens opacos nos tamanhos; requests com esses itens não treinam a razão texto/token, pois bytes criptografados não equivalem aos tokens internos. |
| Compactação | Manual/overflow usam o resumidor isolado; background conserva fingerprint e verifica benefício estimado; hard threshold sem resumo pronto usa extrato local. Root e grupos assistant/tools já tinham retenção própria. | Corrigidos último pedido, autoridade, duplicação do checkpoint, extrato e recuperação. Mantidos thresholds, break-even, limites, contabilização de tentativas e invalidação de evidência da etapa 2. |
| Persistência/retomada | `durable_provider_history` reconstrói entradas user/assistant textuais, aplica checkpoint apenas com âncora/fingerprint/cadeia compatíveis e recusa metadados de tools que não sabe reconstruir. | Mantida recusa segura. Não é restauração integral de um request protocolar interrompido. Nenhum formato JSONL foi migrado. |
| Troca de modelo/provider | O host mantém o histórico visível e cria adapters conforme a seleção. Novo run prepara e mede o request para sua janela. | Itens opacos só são aceitos pelo adapter de origem e modelo exato. Nova instância descarta esse estado; texto e ferramentas permanecem disponíveis conforme o histórico do host. |

## Mudanças implementadas

### C3-1 — continuidade de ferramentas e reasoning Responses

**Verificado.** O runtime substituía campos de write/patch bem-sucedidos por `[omitted N bytes]`. Isso alterava o histórico da chamada efetivamente executada e podia obrigar a reler o arquivo para descobrir a mudança. A substituição foi removida. IDs, nomes e argumentos permanecem no histórico, sujeitos à redaction já existente.

O parser Responses já solicitava `reasoning.encrypted_content`, mas `response.output_item.done` de reasoning só produzia o encerramento visual. Agora conserva o item criptografado em um campo próprio de `ProviderMessage`, separado de content/content_blocks. O normalizador o entrega ao loop; o adapter o recoloca no input seguinte. O item JSON e os valores de seus campos são preservados; não se promete identidade dos espaços/ordem de chaves da serialização JSON original.

Esse campo não vira evento da UI/JSONL, entrada do resumidor, transcrição de recuperação ou conteúdo de Debug. Não passa pela substituição textual de segredos, que corromperia um bloco criptografado; os demais campos continuam redigidos. Respostas com esses itens não são elegíveis ao cache local de respostas.

O escopo usa a identidade process-local já existente da configuração do adapter e o modelo exato. Não acrescenta credenciais, ciphertext ou outro segredo à chave de roteamento de prompt cache. O início do loop remove estados de outra origem; a construção do wire também filtra a origem. Chat e Messages não recebem esses itens.

**Fixture:** o teste existente `codex_subscription_responses_execute_tool_and_send_function_output` agora executa write em localhost. Verifica conteúdo escrito, argumentos exatos, call_id/output, item criptografado no segundo POST, exclusão de logs/Debug/resumo e rejeição ao trocar adapter/modelo/conta. Confere também igualdade de instructions, tools e prompt_cache_key entre os dois requests.

**RED controlado:** desativar temporariamente apenas a passagem do estado opaco ao histórico fez a mesma fixture falhar: o segundo input não tinha o item (`None`). O arquivo foi restaurado byte a byte em finally. A fixture passou com o fluxo implementado. A prova de ausência original também está no parser de entrada, que não tinha representação para esse item.

O contrato de continuidade foi confrontado com a [documentação oficial de reasoning](https://developers.openai.com/api/docs/guides/reasoning), consultada nesta sessão: cadeias consecutivas de ferramentas precisam preservar os itens pertinentes desde a última mensagem do usuário. Isso orienta o corte do histórico; não comprova ganhos comerciais de qualidade ou cache.

### C3-2 — compactação conserva solicitação e autoridade

**Verificado.** A seleção por tamanho podia deixar o assistant/tool recente e resumir o pedido que o originou. A fixture com pedido recente de 80.000 caracteres selecionava índice 3; agora seleciona índice 2 e conserva esse pedido com suas ações. O orçamento de retenção recente é uma preferência; não autoriza amputar esse conjunto.

Mensagens system/developer na parte substituída são preservadas como mensagens, junto do root original e do sufixo. Não são promovidas a partir de tool output nem recuperadas do texto de um resumo.

Uma continuação ativa de ferramentas com reasoning Responses conserva sua cadeia desde o último pedido. Quando toda a história útil pertence à primeira solicitação ainda ativa, ela não é tratada como histórico descartável para satisfazer o threshold. Se o conjunto intacto exceder a janela/reserva, o limite de contexto ainda pode encerrar a execução; não se força uma falsa economia removendo estado necessário.

### C3-3 — checkpoint único no request do resumidor

**Verificado.** Depois de aplicar uma compactação, o histórico tinha `[Compacted context]` e o handle também guardava o mesmo checkpoint. A construção seguinte incluía ambos. Agora retira a cópia idêntica do trecho transcrito e mantém uma única cópia explícita como checkpoint anterior.

A cópia preservada fica fora do trecho sujeito ao corte do transcript. Assim, a deduplicação não faz o checkpoint desaparecer caso o miolo do transcript precise ser truncado. Checkpoints diferentes não são fundidos ou classificados. A fixture existente falhou com duas ocorrências e passou com uma.

### C3-4 — extrato de emergência e recuperação

**Verificado.** O extrato anterior parava após os primeiros 8 KiB e formatava basicamente role/content. Podia perder o fim do histórico e a identidade de chamadas. Agora reutiliza a formatação de transcrição, incluindo nomes, IDs, argumentos e blocos textuais; conserva início e fim dentro do limite UTF-8 e sinaliza omissões.

A única nova fixture de teste verifica que uma decisão recente de bloqueio e seu `call-critical` sobrevivem ao extrato de um histórico longo. Antes falhava no ID ausente; depois passou. O extrato continua identificado como extração local, sem alegar a fidelidade de um resumo semântico.

Ao aplicar qualquer um dos três caminhos de compactação do loop, o runtime com ArtifactStore arquiva o texto visível completo da parte removida e acrescenta ao checkpoint seu path para `read`. Usa o armazenamento endereçado por conteúdo e a ferramenta que já existiam. Não há índice de memória, recall novo ou segundo resumidor. Erro ao guardar o artefato impede substituir o contexto por uma referência inexistente.

A fixture existente de hard threshold foi ampliada: o arquivo contém integralmente o texto antigo e o contexto seguinte contém o path correspondente. O core construído sem ArtifactStore conserva o fallback sem referência; o executor CLI/TUI normal configura o store em `.slim/artifacts` ou no diretório explícito.

## Economia e decisões de preservação

- O ganho medido é a remoção de uma cópia de checkpoint na fixture; os números da execução constam na validação abaixo. Isso mede payload do prompt do resumidor, não economia total de uma tarefa.
- Argumentos completos e estado opaco podem aumentar o payload. Esse custo foi aceito para conservar informação efetivamente necessária. Não houve benchmark de chamadas totais, releituras, qualidade, latência, preço ou taxa de cache comercial.
- Prefixos nativos e schemas foram mantidos. A fixture verifica estabilidade; não comprova um cache hit remoto. Prompt cache do provider permanece distinto do cache local de respostas.
- Não foram adicionados `previous_response_id`, WebSocket, compactação nativa remota, novos parâmetros de reasoning, memória semântica, classificadores ou cópia da arquitetura do Pit. Benefício/compatibilidade dessas mudanças não foi demonstrado neste transporte.
- O ganho adaptativo do estimador continua limitado ao seu ciclo de vida. Runtime normal é criado por execução; não foi criado um novo estado global/persistido para calibrá-lo entre processos.

## Dependências e limitações restantes

1. **Transporte/adapter:** a conservação opaca cobre itens reasoning com encrypted_content emitidos em `response.output_item.done` pelo parser Responses compartilhado. Não é replay genérico de todos os tipos de output, IDs de mensagens, annotations, phase ou tipos de eventos futuros. Function calls continuam no formato normalizado existente. Streams que só entreguem estado no output agregado final precisam de tratamento próprio e fixture antes de serem considerados cobertos.
2. **Ciclo de vida:** a identidade é por instância de configuração, não uma identidade persistente de conversa. A CLI/TUI recria adapters entre execuções; portanto, mesmo voltar ao mesmo nome de modelo não restaura o reasoning opaco anterior. Conservar isso entre execuções/restarts depende de transporte/adapter com origem estável e persistência protocolar adequada. Não se reutiliza ciphertext entre contas por inferência do nome do modelo.
3. **Outros protocolos:** Anthropic thinking/signature/redacted_thinking e estados equivalentes de outros providers não foram implementados nesta etapa. A coleta visual de ReasoningDelta não prova suporte a seu replay. Chat/Anthropic e wrappers tiveram regressões offline executadas, sem afirmação de paridade desses estados.
4. **Retomada durável:** continua textual e conservadora. Não retoma automaticamente uma ferramenta interrompida a partir apenas de texto/summary. Checkpoints antigos não ganham dados que nunca foram persistidos.
5. **Recuperação:** o artefato guarda o texto que ainda existia no histórico, já redigido; não recria saídas removidas antes desta etapa, conteúdo original já paginado/truncado, reasoning opaco ou anexos binários. Resultados grandes anteriores podem apontar para seus próprios artefatos. `read` conserva seus limites de arquivo/página; não se promete leitura irrestrita de artefatos arbitrariamente grandes. Retenção/limpeza desses arquivos segue o store existente, sem coletor novo.
6. **Resumo e orçamento:** resumo por modelo continua sujeito a omissões semânticas. O extrato local é deliberadamente incompleto; o caminho do artefato permite recuperar o texto removido, mas não força o modelo a fazê-lo. Estimativa de tokens não equivale à contagem interna dos itens criptografados ou imagens.
7. **Projeto/skills:** nenhuma promessa de ingestão automática de todas as instruções do repositório. O host/modelo precisa descobrir e ler os arquivos aplicáveis pelo fluxo atual. Também não foi criado um removedor semântico de instruções de skill que apareçam parafraseadas em resumos.
8. **Compatibilidade Rust:** `ProviderMessage` ganhou um campo e `ProviderEvent` uma variante. O workspace inteiro é verificado; consumidores Rust externos com struct literals ou matches exaustivos precisam se ajustar. Os formatos de sessão persistidos não foram alterados.

## Validação e entrega

- RED da compactação: `cargo test -p slim-core --test compaction` → **13 passed / 3 failed**, exit 101; último pedido descartado, checkpoint duplicado e decisão/ID recente ausente do extrato. GREEN posterior: **16 passed / 0 failed**.
- RED controlado Responses: fixture `codex_subscription_responses_execute_tool_and_send_function_output` com passagem do estado desativada → **0 passed / 1 failed**, exit 101. GREEN com a implementação → **1 passed / 0 failed**, exit 0; também coberta pelo gate integral final.
- Focados: `cargo test -p slim-core --test compaction --test agent_loop --test provider_cache_multimodal --test provider_http --test provider_adapters --test context_budget` → exit 0. O gate integral posterior verificou novamente essas suítes e os testes CLI/TUI/retomada/wrappers.
- Medição: `cargo test -p slim-core --test compaction summary_prompt_is_structured_chains_checkpoint_and_bounds_tool_results -- --nocapture` → **before=145 bytes, after=101 bytes, avoided=44 bytes**. Compara o prompt corrigido ao payload anterior reconstruído para o mesmo checkpoint e transcript. É uma fixture pequena e determinística; não representa um percentual de economia de tarefa ou tokens reais.
- `cargo clippy --workspace --all-targets -- -D warnings` → exit 0.
- `.\refresh-slim.ps1 -Test` executou `cargo test --workspace` sem filtros → **1101 passed / 0 failed / 1 ignored / 88 suítes / 0 compiler warnings**, incluindo doc-tests. Contagem somada do log desta sessão. O único ignored é ConPTY físico.
- Build release e instalação → exit 0. **`OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 19:43:26)`**.
- Target/PATH conferidos: **15.156.224 bytes**, SHA-256 idêntico **`3EE4CC87AEAE7731E05DCC4D354E5C2AA43392D5725ED57DF5FD80E30AFE8637`**. Instalado: `--version` → `slim 0.1.0`, exit 0; `--headless --fake 'etapa 3 offline'` → `success`, exit 0.
- `rustfmt --edition 2021 --config skip_children=true --check` nos sete arquivos → exit 0. `git -c core.safecrlf=false diff --check` global → exit 0.
- `cargo fmt --all -- --check` → exit 1 por formatação preexistente fora dos sete arquivos, incluindo OAuth/xAI, testes TUI e LSP. Não foi aplicada formatação global. A comparação com as cópias de entrada confirmou a preservação desses fontes.
- Toolchain conferido: **rustc 1.98.0 (88d9e12ae 2026-08-18)**, Scoop/MSVC. Nenhuma chamada paga ou autenticação live executada.

Logs auxiliares: `%TEMP%\slim-harness-etapa3-20260904/{compaction-red,responses-red,responses-green,focused-2,measurement,clippy,refresh,fmt-workspace}.log` e `stage-diff-counts.json`. O texto acima contém a evidência necessária à continuidade, sem depender da retenção dos temporários.

A fixture TUI existente sem diretório de artefatos explícito gerou um arquivo de 2.000.068 bytes em `crates/slim-cli/.slim/artifacts`. O arquivo identificado foi movido para `tui-fixture-artifact` no diretório temporário de evidência, sem excluir dados do usuário ou deixar artefato gerado no diff.

### Checklist de RULES.md §4

- [x] Rodei o que afirmo ter rodado; comandos e saídas registrados acima.
- [x] Números contados nesta sessão; autoria comparada à entrada, não atribuída a todo o diff contra HEAD.
- [x] Arquivos/símbolos de evidência lidos nesta sessão; nenhuma referência caminho:linha inventada.
- [x] `cargo test --workspace` verde: 1101 passed, 0 failed, 1 ignored, 0 compiler warnings.
- [x] `.\refresh-slim.ps1 -Test` executado, imprimiu `OK:`; executável instalado conferido.
- [x] Índice, status e contagens correntes atualizados; números antigos permanecem apenas nos registros históricos de seus gates.
- [x] Incertezas declaradas: protocolos/retomada opaca incompletos, qualidade/cache comerciais não medidos, console físico e fmt global fora da validação desta etapa.

Não aplicáveis: alteração do contrato visual, teste ConPTY físico, chamada comercial, migração de JSONL, publicação externa e regeneração do ZIP de distribuição. O deploy realizado é o binário local exigido por RULES.md.

## Continuidade

Preservar o checkpoint único fora do trecho truncável, a cadeia do pedido recente, os argumentos de tools, a recuperação por artefato e a separação entre apresentação e estado opaco. Uma etapa de transporte deve partir das oito limitações acima, especialmente origem estável entre execuções, outputs Responses completos e persistência de protocolo. Usar fixtures de troca/retomada antes de habilitar replay; não alegar que compartilhar o nome do modelo autoriza compartilhar estado de uma conta. Não há benefício demonstrado que justifique transplantar o harness do Pit ou trocar o fluxo atual por um sistema de memória.
