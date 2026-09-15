# Revisão da economia de tokens nativa do Slim

Data: 2026-09-04, America/Sao_Paulo. Checkout: `D:\Slim`.

## Resultado e alcance

**Verificado:** três ajustes pequenos no núcleo de ferramentas retiram conteúdo redundante: caminhos absolutos repetidos em `list`, texto do padrão repetido por ocorrência em buscas múltiplas e um terminador LF indevidamente conservado pela busca. O conteúdo útil permanece disponível na própria resposta. Nenhum corte novo de histórico, resultado ou orçamento foi introduzido.

Na fixture `list → search → read → conclusão`, a soma dos quatro corpos de request diminuiu **6.100 bytes** em cada uma das dez combinações de rota/protocolo examinadas, cobrindo os sete providers. Isso inclui a retransmissão dos resultados no histórico. Não é contagem de tokens, economia faturada ou prova de qualidade de modelos comerciais.

O núcleo examinado já tem várias economias úteis. Não encontrei fundamento para uma compressão ampla adicional que preserve as condições solicitadas. O resultado é uma melhoria incremental demonstrada, sem afirmar um teto absoluto de otimização.

## Escopo, fontes e autoria

Foram lidos `AGENTS.md`, `RULES.md`, os relatórios das etapas [1](HARNESS-SLIM-ETAPA-1.md), [2](HARNESS-SLIM-ETAPA-2.md), [3](HARNESS-SLIM-ETAPA-3.md), [4](HARNESS-SLIM-ETAPA-4.md) e [5](HARNESS-SLIM-ETAPA-5.md), os [quick wins posteriores](QUICK-WINS-AGILIDADE-NATIVA-SLIM.md), o [backlog de economia](AUDIT-ECONOMIA-TOKENS.md) e os registros de [calls por tarefa](AUDIT-CALLS-PER-TASK-2026-09-04.md) e [busca/leitura](AUDIT-SPEED-CALLS-TOOLS-READS-2026-09-04.md). Seus números históricos não foram usados como medições atuais. Os mecanismos abaixo foram conferidos no código desta sessão.

Status, diffs relevantes, conteúdos dos arquivos candidatos e hashes dos fontes foram registrados antes de editar. O checkout já continha trabalho amplo de harness, OAuth/xAI, TUI e ferramentas. Nenhum reset, restore, stash, branch, commit, push ou limpeza foi executado. As cópias de comparação ficaram na memória da sessão, sem backups ou nova infraestrutura no projeto.

Somente cinco arquivos Rust mudaram em relação à entrada:

- [`tools/mod.rs`](../crates/slim-core/src/tools/mod.rs): apresentação de `list`.
- [`tools/search.rs`](../crates/slim-core/src/tools/search.rs): legenda condicional e tratamento de LF.
- [`tool_contracts.rs`](../crates/slim-core/tests/tool_contracts.rs): extensão da paginação/listagem e uso direto do path em `read`.
- [`search_bounded.rs`](../crates/slim-core/tests/search_bounded.rs): formatos LF/CRLF/sem terminador, página pequena e legenda por página.
- [`agent_loop.rs`](../crates/slim-core/tests/agent_loop.rs): sequência completa com captura HTTP e comparação dos adapters.

Comparação com a entrada: **33 linhas adicionadas e 6 removidas em produção**; dois testes novos e extensões de testes existentes. Nenhuma dependência, configuração, framework ou adapter foi acrescentado. O diff contra HEAD também contém trabalho anterior e não representa autoria desta revisão. A conferência de hashes após o gate encontrou somente os cinco fontes acima alterados.

## Diagnóstico do custo real

| Área examinada | Comportamento confirmado e decisão |
|---|---|
| Instruções | `ProviderConfig::effective_system_prompt` fornece o núcleo compacto ou o override explícito. Nenhum corte adicional: autoridade, escopo, validação e encerramento têm função operacional. Não confundir as instruções fornecidas a este Codex com ingestão automática de todos os documentos pelo produto Slim. |
| Schemas | `Runtime::provider_tool_definitions` usa registry/modo, omite `code_intel` sem backend e expõe interação quando disponível. A lista é construída uma vez antes do loop. As descrições recentes de CRLF, unicidade e precondições evitam chamadas inválidas; foram preservadas integralmente. |
| Skills e estado da tarefa | Descoberta e invocação são separadas; corpos de skills não são todos injetados no prefixo. Todo/skill e interação têm rotas próprias. Não criar seleção por modelo ou catálogo paralelo para economizar alguns schemas. |
| Histórico | O loop conserva mensagens, argumentos completos, IDs e resultados; o histórico é retransmitido pelos adapters. Esse é o multiplicador dos ganhos encontrados: `list` aparece em três requests seguintes e `search` em dois na fixture. |
| Resultados e artefatos | `materialize_results` e `prompt_output` mantêm limite de apresentação e referência ao artefato. Shell conserva início/fim e marcadores. `read`, `list` e `search` já paginam. Nenhum teto foi reduzido; resultados vazios, falhos e truncados não viraram sucesso completo. |
| Busca/leitura/diagnósticos | Busca múltipla já compartilha scan, snapshots e cursor; leitura tem páginas/checkpoints. A busca já mostra paths relativos. Diagnósticos conservam localização, severidade, mensagem, versão/completude e staleness: não tratei esses metadados como lixo. |
| Duplicação e progresso | O loop só substitui resultado bem-sucedido por marcador menor quando o conteúdo idêntico continua no contexto ativo; compactação invalida essa memória. Governor/receipts tratam progresso e mudanças observadas separadamente. Esta revisão não ampliou deduplicação entre operações ou estados. |
| Falhas e recuperação | Status real do shell, resultados completos do lote, barreiras de mutação e proteção de retries permanecem. O transporte não ganhou retry cego; o caminho condicionado de overflow/compactação continua existente. |
| Compactação | Pedido recente, raiz, autoridade e grupos assistant/tools têm retenção própria; checkpoint idêntico já aparece uma vez no resumidor. Background usa estimativa de custo/benefício; extrato local e artefato de recuperação continuam explícitos. Reduzir o texto resumido por idade poderia aumentar releituras. |
| Finalização | Término normal do provider encerra diretamente; paradas por limites têm seu caminho de finalização. Nenhuma rodada necessária foi removida, nem esforço/modelo/budget alterado. Políticas anteriores não são ganhos desta revisão. |

O percurso verificado é `headless/TUI → run_agent_loop_with_messages → tools/histórico → adapter.prepare_* → PreparedProviderRequest → HTTP`. Os sete braços de `slim-cli/src/headless.rs` usam o loop comum e `with_shared_transport`.

Em [`provider.rs`](../crates/slim-core/src/provider.rs), o builder materializa o `Value`, incluindo opções e intenção de cache; `from_http_body` serializa o JSON final. `PreparedProviderRequest` conserva seus bytes e `serialized_chars`. `send_and_read_sse` move esse mesmo corpo para `builder.body(request.body).send()`. Construir/copiar uma estrutura local não implica uma chamada extra nem prova que esse conteúdo foi enviado duas vezes.

A nova fixture captura integralmente o corpo recebido em localhost, compara-o byte a byte com o prepared request Chat e confere que `ContextSnapshot.serialized_chars` corresponde aos caracteres capturados. Nos demais adapters, compara os corpos efetivamente preparados para envio, sem executar rede comercial.

### O que cada medida significa

- **Conteúdo construído:** strings/JSON internos, inclusive dados de execução que podem não entrar no prompt.
- **Conteúdo enviado:** corpo JSON serializado; diferenças de escape, Unicode e envelope fazem bytes do texto diferirem de bytes do corpo. Cabeçalhos HTTP não entram nas tabelas abaixo.
- **Componentes locais:** `provider_request_components` mede valores serializados de system, tools, histórico e resultados. Sua soma não é uma contagem de tokens nem necessariamente o tamanho de todo o envelope.
- **Estimativa de entrada:** `AdaptiveTokenEstimator` começa em aproximadamente 3,5 caracteres por token e pode calibrar a razão por provider/modelo com usage elegível. Não é tokenizer; multimodal e reasoning opaco exigem limites próprios. Não convertemos o ganho em bytes em tokens estimados nesta entrega.
- **Usage do servidor:** `RequestUsage` separa entrada sem cache, escrita/leitura de cache, saída, reasoning e uso desconhecido. Reasoning não deve ser somado outra vez à saída quando já for seu componente; o total atual soma entrada e saída. A fixture nova não fornece usage real.
- **Cache de prompt:** campos de intenção/breakpoints e prefixos estáveis podem permitir reaproveitamento remoto. Isso não comprova hit ou desconto. Foram conservados na comparação dos JSONs.
- **Cache local de resposta:** é opt-in na biblioteca e permanece desligado no caminho normal `with_shared_transport`, que passa `None`. Não se usou replay para simular redução de inferência.
- **Cobrança:** depende da medição e do contrato do serviço. Não foi consultada nem inferida a partir do payload.

## Melhorias e provas

### E1 — listagem utilizável sem repetir a raiz absoluta

**Verificado:** `execute_list` renderizava diretamente `entry.display()`, com o path absoluto/canônico de cada entrada. Em Windows, isso inclui o prefixo estendido e a raiz, repetidos até em uma listagem de arquivos irmãos. As ferramentas de arquivo pedem paths relativos ao workspace.

A apresentação agora reutiliza `search::display_path` com o workspace canônico, o mesmo mecanismo que a busca já empregava. `src/ação-00.rs` continua identificando o arquivo inteiro dentro do workspace; não se retorna apenas o basename. Paths internos, stamps, snapshots, cursor, ordenação, totais e validação de contenção permanecem inalterados. A API de baixo nível `list_directory` continua retornando seus paths como antes.

O teste de paginação falhou antes com um path absoluto onde era esperado `file-00.txt` (EXIT=101). Após a correção, conserva páginas, snapshot diante de alteração externa, erro de cursor incompatível e atualização por nova listagem. A extensão com `other/ação.txt` usa o resultado diretamente em `read` e recebe `1: nested evidence`. Não há chamada de conversão ou lookup adicional.

Na fixture completa, 20 entradas: **1.579 → 339 bytes de texto**. A magnitude depende da raiz usada nesta execução; uma raiz mais curta dá economia menor. Evitar o prefixo também elimina a necessidade de o modelo converter manualmente o path; não foi medida a frequência com que modelos reais erravam essa conversão.

### E2 — padrão declarado uma vez na própria página

**Verificado:** `format_search_batch_page` acrescentava `[pattern N: texto inteiro]` em cada hit, mesmo repetindo dezenas de vezes o mesmo padrão dentro de um snapshot.

Agora a página pode declarar `[pattern N: texto]` uma vez para cada padrão presente, seguido de hits com `[pattern N]`. A legenda fica antes dos hits e é repetida na página seguinte. O código compara os bytes da legenda com os bytes dos rótulos que ela substituiria; se não diminuir, conserva a apresentação anterior. Busca de padrão único e páginas pequenas não ganham legenda desnecessária.

Arquivos, linhas, texto das ocorrências, índice do padrão, ordem, multiplicidade, cursor, contagens, aviso de snapshot limitado e exclusões de busca permanecem. Não foram agrupadas ocorrências iguais de padrões diferentes. A transformação acontece apenas na apresentação de uma página; não deduplica observações feitas em momentos diferentes.

A regressão nova percorre duas páginas com padrões Unicode e altera o arquivo entre elas: cada página continua autossuficiente e a segunda conserva o snapshot. A economia isolada dos rótulos foi **498 bytes por página** nessa fixture, antes de considerar o erro LF. O caso pequeno existente conserva exatamente seus dois rótulos inline.

### E3 — terminador LF removido uma vez

**Verificado:** `strip_line_ending_str` encadeava a retirada de `\n` e `\r`, mas o último `unwrap_or(line)` usava a linha original. Em LF sem CR, a segunda retirada falhava e recolocava o `\n`. O join de hits acrescentava outro newline: uma linha vazia extra por ocorrência.

O resultado intermediário agora é vinculado antes da segunda retirada. A regressão compara o mesmo resultado para LF, CRLF e linha final sem terminador. Texto e posição permanecem, sem normalização de espaços ou alteração dos arquivos pesquisados. O caminho de patch CRLF não foi modificado.

Na fixture completa, E2+E3 reduzem o texto da busca de **4.471 → 3.619 bytes**. Dos 852 bytes evitados, 812 são rótulos repetidos e 40 são terminadores LF extras. O JSON escapa esses terminadores; por isso o ganho no corpo não deve ser calculado apenas pela diferença de texto bruto.

## Comparação da tarefa e alcance entre providers

Fixture `compact_tool_results_preserve_a_complete_task_across_provider_wires`, em `agent_loop.rs`: 20 entradas de diretório, um arquivo com 20 linhas e duas buscas literais combinadas. Executa **quatro requests e três tools**, termina em `ProviderCompleted`, lê a implementação a partir do path retornado e conserva os bytes do arquivo. Não há artefato, compactação ou chamada de recuperação nessa sequência.

O **depois** de Chat foi capturado no servidor localhost. O **antes** foi reconstruído a partir das renderizações anteriores verificadas no fonte de entrada: raiz absoluta de cada entrada, rótulo completo por hit e LF indevidamente retido. As mesmas mensagens, ações válidas, argumentos e resultado final foram usados na comparação. Não é uma execução autônoma anterior com um modelo escolhendo suas próprias ações.

Para cada adapter, preparam-se os quatro prefixos desse histórico nas duas representações. Após substituir apenas os dois conteúdos modificados, os JSONs devem ser iguais integralmente: IDs, argumentos, schemas, instruções, opções de geração e intenção de cache. As dez combinações cobrem as três opções de Go e as duas de Command Code; não pretendem cobrir todos os modelos dos catálogos.

| Rota / modelo de fixture | Protocolo | Soma antes (bytes) | Soma depois (bytes) | Evitados (bytes) |
|---|---|---:|---:|---:|
| OpenAI compatible / fixture-model | Chat | 47.245 | 41.145 | 6.100 |
| Codex / gpt-5.6-terra | Responses | 47.673 | 41.573 | 6.100 |
| Anthropic / claude-sonnet-4-6 | Messages | 46.669 | 40.569 | 6.100 |
| xAI / grok-4.5 | Responses | 47.817 | 41.717 | 6.100 |
| ClinePass / cline-pass/qwen3.7-max | Chat | 47.385 | 41.285 | 6.100 |
| OpenCode Go / deepseek-v4-flash | Chat | 47.365 | 41.265 | 6.100 |
| OpenCode Go / gpt-5.6-luna | Responses | 47.833 | 41.733 | 6.100 |
| OpenCode Go / minimax-m3 | Messages | 46.641 | 40.541 | 6.100 |
| Command Code / gpt-5.6-sol | Chat | 47.341 | 41.241 | 6.100 |
| Command Code / claude-sonnet-4-6 | Messages | 46.805 | 40.705 | 6.100 |

Esses valores pertencem à execução focada desta sessão. O path temporário contém PID; páginas com cursor também têm identificador variável. O teste exige redução e preservação de conteúdo, não fixa esses tamanhos como constantes universais. A igualdade dos ganhos entre protocolos aqui vem do mesmo texto/escape substituído na mesma sequência; não prevê percentuais idênticos de tokens por modelo.

## Preservações, propostas descartadas e espaço restante

Mantidos: pedido e restrições, autoridade existente, histórico causal, IDs e argumentos completos, resultados de erro/validação, limites e marcadores, recuperação existente, reasoning Responses opaco no ciclo suportado, redaction, autenticação, caches e as melhorias recentes de edição. O gate reexecuta suas regressões. Nenhuma mudança de esforço, modelo, budget, política de parada ou verificação do produto foi usada como economia.

| Proposta examinada | Motivo para não implementar |
|---|---|
| Encurtar todas as descrições ou impor limite de palavras | Requisitos de edição recém-explicitados podem evitar erro e retrabalho. Não há evidência de que cortá-los melhore a tarefa. |
| Tools selecionadas dinamicamente por turno | Registry já filtra por modo/capacidade. Prever a ferramenta errada pode custar outra chamada e variar o prefixo de cache. |
| Apagar outputs antigos ou argumentos de write/patch | Idade/tamanho não demonstram irrelevância; reler/reconstruir pode custar mais. Argumentos são registro do que ocorreu. |
| Deduplicar texto idêntico entre quaisquer estados | Igualdade textual não estabelece mesma observação. Preservada a separação entre apresentação, evidência e progresso. |
| Resumir mais cedo, em cascata ou com modelo menor | Compactação já tem custo/benefício e preservação de âncoras. Não se demonstrou saldo melhor de chamadas e fidelidade. |
| Diminuir caps de leitura, shell e diagnósticos | Pode retirar a evidência necessária e exigir paginação/reexecução. Erros, fins de logs e estado do servidor têm valor. |
| Retirar metadados do artefato ou substituir mais texto por paths | Não foi demonstrado ganho de tarefa que compense ampliar esse contrato. Recuperação pode exigir ferramentas adicionais e permanece sujeita a paths/limites de leitura. |
| Novo cache de respostas, serialização persistida ou compressão HTTP | Replay pode ficar obsoleto; cópias locais e compressão de transporte não equivalem a reduzir tokens vistos pelo modelo. |
| Recursos exclusivos de fabricante como solução geral | Não atendem às sete rotas. Nenhum `previous_response_id`, novo estado remoto ou thinking adicional foi habilitado. |
| Menos reasoning, budgets menores ou encerramento antecipado | Violariam o objetivo; nenhuma alteração desse tipo foi feita. |

**Avaliação:** o espaço seguro encontrado é localizado em apresentação determinística. Dentro dos fluxos examinados, cortes grandes adicionais exigiriam saber o que cada modelo realmente usa e medir releituras, erros e qualidade da tarefa. Há espaço possível em tarefas muito verbosas, mas seu benefício líquido não foi comprovado. Não há fundamento nesta revisão para prometer economia ampla adicional ou afirmar que o Slim atingiu um limite absoluto.

Limitações preservadas do produto: reasoning opaco não é replay durável universal entre instâncias/contas; Messages/thinking e retomada completa têm os limites descritos nas etapas anteriores. Artefatos recuperam apenas o texto que efetivamente foi guardado; não tornam gratuita ou irrestrita a recuperação. Nada nesta revisão amplia essas garantias.

## Validação desta sessão

Toolchain conferido: **rustc 1.98.0 (88d9e12ae 2026-08-18)**, Scoop/MSVC; `RUSTC` e `RUSTDOC` alinhados apenas no processo.

| Comando / cenário | Resultado |
|---|---|
| `cargo test -p slim-core --test tool_contracts list_tool_paginates_directory_entries -- --nocapture`, antes | 0 passed / 1 failed, EXIT=101; path absoluto reproduzido. |
| `cargo test -p slim-core --test search_bounded repeated_search_patterns -- --nocapture`, antes | 0 passed / 1 failed, EXIT=101; rótulos e linhas extras reproduzidos. |
| `cargo test -p slim-core --test tool_contracts --test search_bounded -- --nocapture`, após | 25 + 5 passed / 0 failed, EXIT=0. A extensão posterior de LF/CRLF/sem terminador foi reexecutada no gate integral. |
| `cargo test -p slim-core --test agent_loop compact_tool_results_preserve -- --nocapture` | 1 passed / 0 failed, EXIT=0; captura, comparação das dez combinações e valores da tabela. |
| `cargo clippy --workspace --all-targets -- -D warnings` | EXIT=0. |
| `rustfmt --edition 2021 --config skip_children=true --check` nos dois fontes de produção e `search_bounded.rs` | EXIT=0; novo bloco de `agent_loop.rs` formatado isoladamente. |
| `cargo fmt --all -- --check` | EXIT=1 por drift preexistente em OAuth/TUI/LSP e trechos antigos de testes; sem formatação global. |
| `cargo test --workspace`, por `refresh-slim.ps1 -Test`, sem filtros | **1111 passed / 0 failed / 1 ignored / 72 suítes / 0 compiler warnings**, incluindo doc-tests. Total somado das linhas `test result: ok` desta execução. |
| `refresh-slim.ps1 -Test` | EXIT=0; testes, release, instalação e `OK:` abaixo. |
| Executável instalado `--version` e `--headless --fake --prompt 'token economy offline smoke'` | `slim 0.1.0` / `success`; ambos EXIT=0. |
| `git -c core.safecrlf=false diff --check` | EXIT=0 na revisão de código e documentos. |

O desenvolvimento da fixture encontrou dois erros de montagem (config sem `Clone` e acesso ao campo `app`) e um índice que ignorava a mensagem system no wire. Corrigidos no teste, sem alterar o produto para atendê-los; a fixture final seleciona a mensagem por role/nome. As falhas intermediárias não são resultados do gate final.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 22:51:24)
```

Target e PATH conferidos: **15.032.832 bytes**, SHA-256 idêntico **`D9DB8BA293C80F4E0EC728D6FF2C0123A44513B7303BF151791326C92C82CE04`**. O ZIP de distribuição não foi regenerado.

Não houve chamada paga, uso de credenciais reais, benchmark de modelos ou teste em outros sistemas operacionais. O único ignored é ConPTY físico. Fixtures comprovam conteúdo e contratos nos caminhos exercitados; não comprovam manutenção de qualidade das decisões de todos os modelos, latência, desconto, cache hit ou ausência universal de regressões.

## ✅ Verificação de Entrega — RULES.md §4

- [x] Rodei o que afirmo ter rodado; comandos e saídas registrados acima.
- [x] Números contados nesta sessão; bytes não apresentados como tokens/cobrança.
- [x] Arquivos e símbolos citados lidos nesta sessão, sem números de linha presumidos.
- [x] `cargo test --workspace` verde: 1111 passed, 0 failed, 1 ignored, 0 compiler warnings.
- [x] `refresh-slim.ps1 -Test` executado e `OK:` conferido; hashes e smokes do instalado verificados.
- [x] Relatório, índices e status atualizados. Referências correntes aos totais/hash anteriores substituídas; relatórios históricos e linhas antigas do tracker preservados como histórico.
- [x] Incertezas e limitações declaradas: fmt global preexistente, qualidade e cobrança não medidas.

Não aplicáveis: alteração visual, novos protocolos, migração de sessão, benchmark comercial, credenciais reais, validação física de terminal, publicação externa e regeneração do ZIP. O deploy local foi autorizado no pedido e executado conforme RULES.md.
