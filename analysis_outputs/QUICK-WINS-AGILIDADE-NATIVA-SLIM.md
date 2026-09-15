# Quick wins de agilidade nativa do Slim

Data: 2026-09-04, America/Sao_Paulo. Workspace: `D:\Slim`.

## Resultado e aceitação

**Verificado:** o Slim agora aceita um trecho LF de `read` para editar um arquivo CRLF uniforme, informa onde um patch foi aplicado e fornece as linhas das ocorrências quando rejeita uma ambiguidade. Os schemas de leitura/escrita explicitam requisitos que antes só eram descobertos ao executar. As mudanças estão no núcleo de ferramentas usado pelos sete providers.

Aceitação atendida: atritos reproduzidos, menor correção no mecanismo existente, regressões locais, continuidade até o request seguinte, gate integral e deploy local. Não houve chamada paga, credencial real, mudança de modelo/esforço/budget, novo framework, cache, ferramenta composta, dependência ou alteração do prompt de sistema. Nenhum commit ou publicação externa.

O ganho comprovado é mecânico: uma edição antes rejeitada passa na primeira tentativa com o mesmo trecho observado; a recuperação de ambiguidade recebe localização utilizável. Não foi medida uma redução geral de rodadas, latência, custo ou retrabalho de modelos comerciais.

## Base, autoria e investigação

Foram lidos `AGENTS.md`, `RULES.md` e os relatórios [etapa 1](HARNESS-SLIM-ETAPA-1.md), [etapa 2](HARNESS-SLIM-ETAPA-2.md), [etapa 3](HARNESS-SLIM-ETAPA-3.md), [etapa 4](HARNESS-SLIM-ETAPA-4.md) e [etapa 5](HARNESS-SLIM-ETAPA-5.md). Também foram consultados os registros posteriores de [calls por tarefa](AUDIT-CALLS-PER-TASK-2026-09-04.md), [busca/leitura](AUDIT-SPEED-CALLS-TOOLS-READS-2026-09-04.md) e [otimização dos testes/build](OTIMIZACAO-TESTES-E-BUILD-SLIM.md). Seus números são históricos, não a validação desta tarefa.

O checkout já continha trabalho amplo de harness, OAuth/xAI, ferramentas, TUI e agrupamento dos testes. Status, diffs relevantes, conteúdos de entrada e hashes dos fontes foram registrados antes das edições. A comparação de hashes identificou somente estes cinco arquivos Rust alterados nesta tarefa:

- [`tools/patch.rs`](../crates/slim-core/src/tools/patch.rs): correspondência CRLF delimitada e informação sobre o resultado.
- [`tools/mod.rs`](../crates/slim-core/src/tools/mod.rs): schemas, erro de precondição e propagação do contexto da falha/resultado.
- [`tools/execution.rs`](../crates/slim-core/src/tools/execution.rs): campo interno opcional para contexto de falha, conservando `ToolError` e observações existentes.
- [`tests/tool_contracts.rs`](../crates/slim-core/tests/tool_contracts.rs): extensão de uma regressão de ambiguidade e dois testes novos.
- [`tests/agent_loop.rs`](../crates/slim-core/tests/agent_loop.rs): uma fixture de continuidade HTTP localhost, reutilizando o servidor/helper existente.

O diff contra HEAD inclui trabalho anterior e não representa autoria desta tarefa. Não foram revertidos, movidos ou limpos arquivos do checkout; os 17 módulos TUI já agrupados permaneceram como estavam. Nenhum relatório das cinco etapas foi reescrito.

## Caminho real e divisão de responsabilidades

| Percurso verificado | Harness impõe ou disponibiliza | Decisão que permanece com o modelo |
|---|---|---|
| Headless/TUI → `execute_provider_turn_async` em `slim-cli/src/headless.rs` | Resolve opções, conserva histórico e adiciona o pedido; os sete braços chamam `run_agent_loop_with_messages` com o mesmo registry/runtime. | Interpretar objetivo, localizar código e escolher ações pertinentes. |
| `ProviderConfig::effective_system_prompt` e `Runtime::provider_tool_definitions` | Envia núcleo nativo ou override e schemas conforme modo; `code_intel` depende de backend, pergunta depende de interação. Núcleo já orienta investigação mínima, validação suficiente e conclusão por critérios. | Escolher ferramentas e contexto; o texto do prompt não prova obediência nem melhor resultado. |
| Adapter → HTTP → normalizador → chamadas | Preserva protocolos Chat, Responses e Messages, argumentos e IDs. Request terminal sem tools conclui; budgets/cancelamento continuam próprios do loop. | Emitir uma ou várias chamadas, manter dependências e avaliar o resultado. |
| Preparação → execução | Argumentos e paths são preparados pelo mecanismo existente. Reads contíguos independentes e validações reconhecidas podem usar concorrência 8; mutações, interações e `--fix` conservam barreiras. | Agrupar trabalho realmente independente; o harness não descobre toda dependência semântica de um comando arbitrário. |
| `read` → `patch` → persistência | `ReadService` pagina com números de linha e LF. Patch lê o arquivo sob a proteção existente, valida unicidade/tamanho e faz substituição observada. A correção atua nesta fronteira de representação. | Fornecer trecho suficiente e alteração correta; omitir os prefixos de linha da apresentação. |
| Resultado → LSP/histórico → próximo request | Mantém receipts, sync de texto do LSP, redaction, IDs, argumentos originais e todos os resultados do lote. Artefatos e truncamento explícito continuam em `materialize_results`/`prompt_output`. | Usar a evidência, corrigir falha e escolher checagem proporcional; sucesso de patch não prova correção funcional. |

**Providers preservados:** OpenAI-compatible e ClinePass usam Chat; Codex e xAI usam Responses; Anthropic usa Messages; Go escolhe entre os três; Command Code escolhe Chat/Messages. Nenhum adapter, credencial, opção de geração ou formato persistido foi alterado. O gate cobre as fixtures existentes desses caminhos; a nova fixture completa usa Chat localhost, não uma matriz nova de todos os providers por cenário.

## Q1 — trecho de leitura utilizável no patch CRLF

**Atrito confirmado:** `ReadService::read_file_range_resolved` remove `\r\n` na apresentação e produz `\n`; `apply_exact_patch_with_content` contava apenas ocorrências byte a byte. A regressão lê duas linhas de uma função em arquivo CRLF, retira só a numeração e envia esse trecho. Antes: `expected exactly one match, got 0`; arquivo não editado.

**Correção:** primeiro permanece a correspondência exata existente. Somente quando ela encontra zero ocorrências, o trecho contém LF sem CR e o arquivo tem exclusivamente terminadores CRLF, o patch representa o trecho como CRLF e conta novamente. Exige exatamente uma ocorrência, converte os newlines da substituição nesse caso e usa a mesma operação protegida de escrita. Não há retry de ferramenta nem segunda escrita.

Espaços, indentação e conteúdo continuam exatos. Arquivos mistos não recebem essa conversão; ocorrências múltiplas continuam sendo erro. Quando uma correspondência exata já existe, sua substituição conserva a semântica anterior. A regra está no schema e na documentação da API; o sucesso informa `LF input normalized to CRLF`.

**Antes/depois:** a sequência de duas ferramentas `read → patch`, antes terminando em rejeição, agora produz os bytes desejados na primeira tentativa de edição. Foram conferidos prefixo Unicode, linhas inseridas, sufixo intocado e ausência de newline final. A necessidade de ao menos uma chamada de correção após aquela rejeição é eliminada nesse cenário; não se mediu quanto um modelo real gastaria para diagnosticá-la.

**Custo e limite:** faz uma verificação adicional do estilo de newline e uma nova contagem somente na condição descrita, sobre o conteúdo já lido. Não acrescenta I/O de releitura, processo ou round-trip. Substituição integral com `write` continua exigindo o texto completo exato, inclusive seus terminadores; não foi relaxada sua precondição.

## Q2 — resultado de edição permite continuar

**Atrito confirmado:** a rejeição por ambiguidade informava apenas `got 2`; o sucesso retornava apenas `patched`. O mecanismo já tinha lido o texto e conhecido a posição, mas descartava essa informação antes do ciclo seguinte.

**Correção:** erro preserva a contagem exata, acrescenta arquivo, linhas iniciais das ocorrências e orientação para incluir contexto inalterado. Exibe até oito localizações com aviso explícito quando há mais; a contagem completa é mantida. Zero ocorrências orienta obter o texto atual. As rejeições de correspondência não escrevem no arquivo.

Sucesso informa arquivo/linha inicial, bytes do trecho removido e inserido e eventual normalização CRLF. Substituição de texto por si mesmo informa conteúdo inalterado. A saída não é um diff completo; deve ser interpretada com os argumentos originais preservados no histórico. Não dispensa testes ou revisão funcional.

**Antes/depois:** a regressão existente de ambiguidade verifica agora linhas `1, 2` e corrige o trecho usando o contexto já disponível, sem uma ferramenta intermediária de localização. A nova fixture HTTP executa `read → patch ambíguo → patch contextualizado → conclusão`: quatro requests programados, três tools com status `[true, false, true]`. Confere linhas `2, 3` no POST após a falha, localização/CRLF no POST após o sucesso, `tool_call_id`, argumentos originais LF e bytes finais do arquivo.

Isso prova que a informação atravessa execução, redaction e histórico até o provider. As respostas são programadas pela fixture; a recuperação não foi escolhida por um modelo. Os quatro requests não são um benchmark antes/depois de decisões autônomas.

## Q3 — contratos explicam requisitos antes da primeira tentativa

**Atrito confirmado em código:** o schema dizia apenas `Write a UTF-8 text file`, com `expected` opcional e sem descrição; a implementação recusa substituir arquivo existente sem `expected` e compara todo o texto. A necessidade só ficava explícita no erro `precondition required`. O schema de `read` também omitia o significado da numeração e seus defaults reais.

**Correção:** `write` explica o requisito condicional e sugere o patch existente para edição localizada; o erro de precondição fornece a mesma alternativa. `read` explicita prefixos de apresentação, offset iniciado em 1 e default de 80 linhas, reutilizando a constante atual. `patch` explicita unicidade, contexto e o caso CRLF; `minLength:1` reflete a validação que já existia.

Nenhum argumento ou default de execução mudou. As descrições crescem para transmitir requisitos úteis, sem redução de contexto ou mudança do prompt de sistema. **Inferência delimitada:** isso permite formular uma chamada válida antes do erro; não demonstra que todos os modelos deixarão de errar. Não foi acrescentado teste que apenas compare o texto das descrições; as precondições e modos são exercitados pelos testes existentes, e o gate mantém a serialização dos adapters.

## Oportunidades descartadas ou não ampliadas

| Candidato | Evidência atual e decisão |
|---|---|
| Criar batch/exec paralelo novo | Já há segmentos concorrentes, dedup de evidência e resultados ordenados. Testes existentes verificam reads em torno da barreira de escrita e validação shell. Remover a barreira confundiria independência com mera proximidade. |
| Read até EOF / custo por linha | O checkout já para após lookahead, usa checkpoints e calcula offset por bytes; otimização posterior já retirou `stream_position` por linha. Não reimplementado. |
| Repetir varredura para cada busca/página | Busca nativa já aceita vários padrões num scan, snapshots e cursor; offset de continuação reutiliza snapshot. Nova busca fresca observa alterações externas. Não criado cache adicional. |
| Adicionar navegação de símbolos | `code_intel` já fornece symbol/definition/references/hover/diagnostics/status e expõe apenas quando há backend. Nenhum caso demonstrado exige outra tool. |
| Busca com trechos de contexto / leitura de vários intervalos | Pode ajudar em outros cenários, mas ampliaria argumentos, snapshots e renderização sem necessidade demonstrada nos quick wins encontrados. Proposta futura separada, não implementada. |
| Retirar falhas/validações para encurtar loop | Etapa 2 já corrigiu status do shell, prefixo de orçamento, resultado completo de lote e evidência causal. Mantidos; não suprimida checagem útil nem adicionada verificação automática universal. |
| Reduzir histórico/prompt ou aumentar dedup | Argumentos completos, pedido recente, checkpoint único e recuperação por artefato já estão presentes. Mais informação foi necessária na edição; menor payload não é o objetivo desta tarefa. |
| Remover rodadas de finalização ou baixar esforço/budgets | Mudaria política e poderia prejudicar conclusão. Nenhuma evidência desta tarefa justifica isso; mantidos. |
| Otimizar rede/tok/s ou trocar provider | Fora do objetivo. Etapas 4–5 e otimizações posteriores foram preservadas, sem repetir experimentos de latência ou chamadas comerciais. |
| Alterar o prompt nativo por hipótese | Já pede escopo mínimo, checagem suficiente e conclusão. Não foi identificado um conflito reproduzido que justificasse acrescentar mais orientação geral. Corrigido o contrato das ferramentas. |

Não há bloqueador conhecido para estas mudanças. Reformulação de busca/contexto, replay opaco durável e catálogos completos continuam assuntos separados; não são etapas adicionais desta entrega.

## Validação desta sessão

Toolchain verificado: `rustc 1.98.0 (88d9e12ae 2026-08-18)`, Scoop/MSVC. Comandos manuais usaram `RUSTC` e `RUSTDOC` do mesmo toolchain somente no processo, sem editar configuração global.

| Comando/cenário | Resultado observado |
|---|---|
| `cargo test -p slim-core --test tool_contracts patch -- --nocapture`, antes da implementação | 2 passed / 3 failed, EXIT=101: trecho CRLF rejeitado, ausência de localizações e contagem incorreta para o trecho LF com duas ocorrências CRLF. |
| `cargo test -p slim-core --test tool_contracts`, após correção | 25 passed / 0 failed, EXIT=0. Inclui cancelamento, containment, stale write, arquivos grandes, ambiguidade e preservação de bytes. |
| `cargo test -p slim-core --test agent_loop patch_recovery_returns -- --nocapture` | 1 passed / 0 failed, EXIT=0; quatro requests localhost e arquivo final conferidos. |
| `cargo clippy --workspace --all-targets -- -D warnings` | EXIT=0. |
| `rustfmt --edition 2021 --config skip_children=true --check` nos três fontes de produção alterados | EXIT=0; nos testes, foram formatados apenas os blocos alterados. |
| `cargo fmt --all -- --check` | EXIT=1, drift preexistente em OAuth, TUI, LSP e trechos de testes fora desta mudança; nenhuma formatação global aplicada. |
| `cargo test --workspace`, dentro de `refresh-slim.ps1 -Test`, sem filtro | **1109 passed / 0 failed / 1 ignored / 72 suítes / 0 compiler warnings**, incluindo doc-tests. O ignored é o ConPTY físico. |
| `refresh-slim.ps1 -Test` | EXIT=0, release e cópia para o PATH concluídos; `OK:` abaixo. |
| `C:\Users\User\bin\Slim.exe --version` e `--headless --fake --prompt 'quick wins offline smoke'` | `slim 0.1.0` e `success`, ambos EXIT=0. |
| `git -c core.safecrlf=false diff --check` | EXIT=0 na revisão de código. |

Durante o desenvolvimento, uma expectativa nova de whitespace foi corrigida: buscar um substring com um espaço em uma linha com dois espaços era uma correspondência exata legítima. O teste passou a incluir a linha anterior para tornar a diferença de indentação real. O produto não foi alterado para satisfazer essa expectativa incorreta.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 22:11:31)
```

`target/release/slim.exe` e `C:\Users\User\bin\Slim.exe`: **15.030.784 bytes**, SHA-256 **`897C2BDCAEC5769AD3BA89E3455D1A556753CEC8426CE1DC2A155DB1ABC48FFE`**, idênticos. O ZIP de distribuição não foi regenerado.

## Qualidade preservada e limites

Unicidade do alvo, limites de arquivo/substituição, contenção de paths, lock/revalidação antes da escrita, cancelamento, modos, budgets, barreiras, receipts, sincronização LSP, histórico e recuperação por artefatos foram preservados. A normalização nova é delimitada a CRLF uniforme, sinalizada e testada; não há correspondência aproximada ou repetição automática de efeito colateral.

O código e os testes foram executados em Windows. Os testes de CRLF não são condicionados ao SO, mas não foram executados em Linux/macOS nesta sessão. Não houve modelo comercial, login real, medição de qualidade autônoma, console físico ou promessa de ausência de regressões em cenários não cobertos. A validação fake do binário é um smoke; a prova de recuperação está na fixture de loop e nos bytes de arquivo.

## ✅ Verificação de Entrega — RULES.md §4

- [x] Rodei o que afirmo ter rodado; comandos e saídas registrados acima.
- [x] Números contados nesta sessão: 1109 passed, 0 failed, 1 ignored, 72 suítes.
- [x] Arquivos/símbolos citados lidos nesta sessão; nenhum número de linha presumido.
- [x] `cargo test --workspace` verde, zero falhas e zero compiler warnings.
- [x] `refresh-slim.ps1 -Test` executado, imprimiu `OK:`; hashes target/PATH e smokes conferidos.
- [x] Relatório, índices e status atualizados; totais anteriores conservados somente em registros históricos de seus gates.
- [x] Limitações declaradas: fmt global preexistente, decisões de modelos reais e outros SOs não medidos. Não aplicáveis: chamada paga/credencial real, mudança visual, gate físico, publicação externa e ZIP.
