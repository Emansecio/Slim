# Harness Slim — etapa 4: transporte dos providers

Data: 2026-09-04. Checkout: `D:\Slim`. Validação: offline/localhost, sem chamadas pagas. As etapas [1](HARNESS-SLIM-ETAPA-1.md), [2](HARNESS-SLIM-ETAPA-2.md) e [3](HARNESS-SLIM-ETAPA-3.md) foram consultadas e confrontadas com o código atual. O diretório existente do projeto é `analysis_outputs`.

## Objetivo, aceitação e limites

Reduzir overhead e fragilidade entre runtime e providers com mudanças comprovadas, preservando protocolos, contabilização, cancelamento e o trabalho preexistente. Aceitação: reprodução local das falhas, correção nas rotas compartilhadas, testes existentes, gate/deploy de RULES.md e continuidade documentada.

Fora do escopo: chamadas comerciais, alteração de modelo/auth, pool ou transporte novo, cache novo, migração de sessões, redesign de CLI/TUI, persistência de reasoning entre contas/execuções e refatoração geral. Não houve subagentes, branch, commit, reset, stash, checkout ou limpeza do trabalho anterior.

## Autoria e tamanho da mudança

O checkout já continha etapas anteriores e alterações em OAuth/xAI, runtime, tools, contexto e TUI. O patch e cópias de entrada ficaram em `%TEMP%\slim-harness-etapa4-20260904`.

Somente dois fontes Rust foram alterados em relação à entrada:

- `crates/slim-core/src/provider.rs`: transporte, framing terminal, classificação de retry e retenção de usage.
- `crates/slim-core/tests/provider_http.rs`: fixtures locais e regressões correspondentes.

Comparação com as cópias de entrada: implementação **110 linhas acrescentadas / 149 removidas**, redução líquida de **39 linhas**; testes **127 acrescentadas / 6 removidas**, incluindo formatação. Duas funções de teste novas; demais verificações ampliam testes/servidor existentes. Nenhuma dependência, framework ou diretório de testes novo. A comparação por hash dos fontes confirmou que os demais arquivos Rust permanecem iguais à entrada.

## Correções implementadas

### T4-1 — término nativo encerra a leitura

**Verificado.** O leitor encerrava imediatamente apenas em `[DONE]`. Responses entregava `response.completed`/usage e Anthropic entregava `message_stop`, mas ambos ainda dependiam de EOF HTTP ou timeout. O runtime recebia uma falha de transporte mesmo depois de um término válido.

`parse_sse_line` agora reconhece o término pelo **wire**, após processar os eventos do adapter: `response.completed`/`response.incomplete` em Responses e `message_stop` em Messages. O corpo HTTP é descartado ao sair. Isso também beneficia xAI, OpenCode Go e Command Code nos respectivos wires, sem confundir a identidade comercial com protocolo.

Chat preserva a leitura após `finish_reason`, pois o usage pode chegar depois; `[DONE]` permanece seu término explícito. `response.incomplete` encerra o transporte, mas conserva seu motivo de parada: não vira resposta bem-sucedida nem autoriza ferramentas truncadas. O requisito existente de um evento `Stopped` válido também permanece; sentinel isolado não passa a ser sucesso.

**RED/GREEN local:** a fixture envia um término nativo em HTTP chunked, não fecha o corpo e aguarda o fechamento pelo cliente. Antes: Responses retorna `Transport { safe_to_retry: true }` após **2003 ms**; Anthropic também falha. Depois: Responses conclui em **31 ms** naquela execução, preservando **7 input / 3 output**, e Anthropic passa. A fixture Responses também executa xAI direto com servidor local e confere o usage. O teste observa o fechamento do cliente, não apenas o retorno de um future.

Esses tempos incluem a criação do runtime de teste e são uma reprodução causal do timeout evitado, não uma amostra estatística de latência comercial. Fechar cedo um corpo HTTP/1 incompleto pode impedir reutilizar aquela conexão; aguardar um EOF indefinido para tentar conservá-la seria o defeito reproduzido. O teste existente de keep-alive com corpo completo continua passando.

### T4-2 — timeout pós-envio não comprova retry seguro

**Verificado.** `connect` cobre também upload e espera de headers; o timeout desse bloco, do stream e do deadline total vinha marcado como `safe_to_retry: true`. O servidor pode já ter recebido/processado o POST. Essa flag também chega à classificação de erro durável, cujo planner usa `safe_to_retry` para permitir outro attempt.

Agora somente um erro de conexão identificado pelo cliente HTTP mantém essa indicação. Timeout de upload/headers, idle, primeiro conteúdo semântico, wall e falha de leitura após resposta são conservadores: `safe_to_retry: false`. Não foi adicionado retry, backoff, header de idempotência ou tentativa automática.

**RED/GREEN:** o servidor lê o request inteiro e fica silencioso; a resposta com heartbeats já começou em outro teste. Ambos ainda falham dentro dos deadlines existentes, mas deixam de afirmar que repetir é seguro. Antes, os asserts dessa classificação falhavam. Não foi reproduzida uma cobrança dupla; a prova é da classificação incorreta e do seu uso pelo contrato de retry.

O cancelamento já pronto ganha prioridade no `select` antes de iniciar o envio. O teste novo exige `Cancelled` sem sequer emitir `Connecting`. A estrutura continua descartando o future HTTP em cancelamento/timeout, sem task de rede destacada criada pelo Slim. Isso encerra a operação local; não prova interrupção de computação ou faturamento no serviço remoto.

### T4-3 — parsing não revarre o prefixo incompleto

**Verificado.** Quando uma linha SSE chegava fragmentada sem newline, `drain_sse` voltava a procurar desde o começo do buffer a cada chunk. Agora um offset local conserva quanto já foi examinado; os bytes continuam retidos para validar UTF-8 e fazer o parse JSON completo, mas a procura por newline visita apenas o trecho novo.

O buffer continua sendo o mesmo vetor, com os limites anteriores de linha (1 MiB) e stream (64 MiB). O processamento do último trecho sem newline usa o mesmo callback de redaction/semântica do loop, eliminando sua cópia. As fixtures existentes de UTF-8 repartido entre chunks, frames consecutivos, EOF sem sentinel, streams parciais, bytes após `[DONE]` e redaction passaram. Não foi criada uma matriz nova de todos os limites do parser.

**Medição sintética local:** foram extraídas as versões de `drain_sse` da entrada e da implementação, compiladas juntas com `rustc --edition 2021 -O`. O parser posterior foi substituído por um stub com `black_box`; o experimento mede busca/buffer, sem JSON, rede, redaction ou modelo. Cada amostra processa 20 linhas ignoradas de 512 KiB, repartidas em 512 chunks de 1 KiB; sete amostras por versão, alternadas antes/depois.

| Versão | Mediana por 20 linhas | Amostras ordenadas, microssegundos |
|---|---:|---|
| Entrada | 502,461 ms | 497991, 501675, 502321, 502461, 508806, 528613, 1122258 |
| Corrigida | 3,434 ms | 3283, 3321, 3347, 3434, 3584, 6408, 6665 |

Esse caso acentua linhas muito grandes e fragmentadas. Não é benchmark do request completo nem evidência de um ganho proporcional no uso normal. O executável/fonte auxiliar permanece no diretório temporário de evidência, não no produto.

### T4-4 — usage acumulado sobrevive à falha da leitura

**Verificado.** OpenCode Go em Chat agrega snapshots cumulativos de usage pelo maior valor, uma particularidade já existente. Esses acumuladores ficavam dentro do future HTTP e só eram emitidos no retorno normal. Um frame inválido depois do usage apagava da contabilização o consumo já observado.

Os mesmos acumuladores agora vivem no escopo que controla erro, deadline e cancelamento. Ao retornar dessa operação, emitem o envelope observado antes de propagar o resultado. O request continua falho/cancelado, sem inserir resposta incompleta no cache. O caminho bem-sucedido continua emitindo uma única contabilização terminal; demais providers preservam seu contrato estrito.

**RED/GREEN:** ampliado o teste de snapshots progressivos com servidor que envia **7 input / 3 output** e depois JSON SSE inválido. Antes, a verificação do usage falha. Depois, há um único POST, o erro permanece, o usage observado chega ao runtime e `RequestCompleted.failed` é verdadeiro. Os testes existentes de snapshots idênticos/progressivos continuam passando. A mudança cobre o mesmo escopo de cancelamento/wall pelo posicionamento do acumulador; a reprodução nova específica é a falha de parsing.

## Percurso examinado e decisões de preservação

| Área | Evidência atual e decisão |
|---|---|
| CLI/TUI → runtime | Os sete braços de `headless.rs::execute_provider_turn_async` usam `with_shared_transport` e o mesmo loop. A TUI projeta eventos desse executor. Nenhum segundo transporte foi introduzido nas superfícies. |
| Preparação/serialização | Adapters nativos e wrappers preparam `Value`, materializam opções/cache nativo e serializam o corpo final. O snapshot usa componentes/caracteres do `PreparedProviderRequest`; esse corpo é movido para o HTTP. O teste do runtime compara agora `serialized_chars` com o corpo lido integralmente pelo servidor e exige uma única construção do request. Não foi encontrada divergência nesse percurso. |
| Trabalho ainda necessário | Cálculo de componentes e fingerprints percorre partes do JSON; histórico e schemas são materializados por request. Isso não equivale a reenviar ou construir duas vezes o POST. Não houve ganho demonstrado que justificasse trocar a contabilização/fingerprints ou criar cache de serialização. Finalização possui conversão própria para reduzir os controles já suportados; não é o caminho de todo delta. |
| Clientes/conexões | Cliente reqwest compartilhado em `OnceLock`, clones compartilham transporte, redirects desabilitados e keep-alive/pool já configurados. A fixture existente exige dois clientes usando a mesma conexão localhost. Preservados os construtores isolados das APIs/testes. |
| Streaming/buffers | Consumo incremental via `bytes_stream`, limites de linha/stream, corpo de erro limitado a 4 KiB e mensagem truncada/redigida. Redaction que segura prefixos de segredos é necessária à segurança e não foi removida para antecipar texto. |
| Progresso útil | `Connecting`, headers, primeiro byte, heartbeats e usage não constituem `FirstSemantic`. O deadline de primeiro conteúdo permanece independente dos heartbeats; a fixture existente o comprova. A UI usa tempo do runtime para tok/s desde a etapa anterior, portanto a remoção da espera terminal evita contaminar também essa duração. Não se afirma medir decode puro do servidor. |
| Backpressure | Fila core limitada com coalescimento; projector TUI em thread separada; lanes de dados/controle e preservação dos eventos causais. Após cancelamento, deltas visuais podem ser descartados para destravar entrega. Testes core/CLI/TUI existentes validam esses contratos. Sem nova fila, task por delta ou polling adicional. |
| Retries/recuperação | O loop não faz retry genérico de HTTP. A repetição por overflow de contexto é limitada e condicionada à ausência de output causal e possibilidade de compactar. O planner durável consulta classe/política da falha. Mantidos esses contratos; corrigida a classificação de segurança do transporte. |
| Contabilização | Uso estimado continua distinto do usage nativo e do cache local de respostas. Resultados falhos não viram sucesso pelo fato de termos observado tokens. Preservados componentes de cache, sequência causal, encerramento e calibração existentes. |

| Rota | Wire e cobertura nesta etapa |
|---|---|
| OpenAI compatible | Chat: localhost, usage posterior ao stop, UTF-8, cache, erro, timeout e request/snapshot. |
| Codex | Responses: fixture local mantém HTTP aberto após término; usage e encerramento conferidos. Regressões de tools/reasoning da etapa 3 cobertas pelo gate integral. |
| xAI | Responses pelo parser compartilhado; fixture nova de término executa o adapter direto em localhost, além do teste de catálogo/rota existente. |
| Anthropic | Messages: `message_stop` encerra a leitura; preservados blocos, usage separado e autenticação por contrato. |
| OpenCode Go | Chat/Responses/Messages pelo wire selecionado; testes existentes e correção específica da retenção do usage acumulado Chat. |
| ClinePass | Chat pelo adapter interno; request/preparação/parser existentes e testes da rota preservados. |
| Command Code | Chat/Messages conforme modelo; testes de adapter/compaction e fluxo compartilhado preservados. |

## Limitações e pendências delimitadas

1. Sem medição de latência, throughput, custo, cache hit ou encerramento de computação em provider comercial. Autenticação/OAuth real e console físico não foram exercitados. Os ganhos reportados são locais.
2. O parser continua orientado às linhas `data:` usadas pelos adapters existentes. Não foi convertido em implementação geral de todos os formatos SSE: dados de um evento divididos em múltiplos campos `data:`, delimitadores somente CR e extensões futuras precisam de fixture/contrato próprio antes de ampliar o parser.
3. Permanecem as limitações de estado opaco da etapa 3: saída agregada Responses sem eventos por item, reasoning/signatures Anthropic, identidade entre execuções e replay durável. Não se reaproveita ciphertext entre contas/modelos com base apenas no nome. Nenhum desses contratos foi habilitado por esta mudança de término.
4. Callbacks de eventos/backpressure são síncronos. Deadlines Tokio não preemptam código síncrono bloqueado; não representam um deadline rígido de CPU/UI. O comportamento comum de cancelamento/fila está coberto por testes existentes, mas não foi demonstrado que todo consumidor arbitrário cumpra um limite de wall enquanto bloqueia o callback.
5. Não houve teste de falhas HTTP/2 reais, mudança de rede ou sessão comercial longa. O reuso comprovado é localhost e os limites/flags são os verificados no código. Ausência de retry geral não é garantia de execução exatamente uma vez no serviço remoto.
6. Formatação global preexistente fora dos dois fontes foi preservada. O ZIP de distribuição não faz parte do deploy local desta etapa.

## Validação desta sessão

- Base: `cargo test -p slim-core --test provider_http --test runtime_abort`, exit 0.
- RED inicial: `cargo test -p slim-core --test provider_http -- --nocapture` → **53 passed / 4 failed**, exit 101: términos nativos e flags pós-envio.
- GREEN inicial: HTTP **57 passed** e abort **8 passed**, exit 0.
- RED de usage: `cargo test -p slim-core --test provider_http opencode_go_progressive` → **0 passed / 1 failed**, exit 101. GREEN posterior: suíte HTTP **58 passed** e abort **8 passed**, exit 0.
- Focados adicionais: `provider_adapters`, `provider_cache_multimodal`, `xai_provider`, `opencode_go_provider`, `command_code_provider`, `clinepass_provider` e `usage_ledger`, exit 0; o gate integral final é a validação do checkout entregue.
- `cargo clippy --workspace --all-targets -- -D warnings` → exit 0.
- `cargo fmt --all -- --check` → exit 1 por drift preexistente fora do escopo; os dois fontes foram formatados individualmente com rustfmt, sem formatar o checkout inteiro.
- `git -c core.safecrlf=false diff --check` → exit 0 na revisão do patch de código.
- Toolchain verificado: rustc **1.98.0 (88d9e12ae 2026-08-18)**, Scoop/MSVC.

- `.\refresh-slim.ps1 -Test` executou `cargo test --workspace` sem filtros: **1103 passed / 0 failed / 1 ignored / 88 suítes / 0 compiler warnings**, incluindo doc-tests. Contagens somadas do log desta sessão; o ignored é o ConPTY físico.
- Build release e instalação concluídos, exit 0: **`OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 20:13:32)`**.
- `target/release/slim.exe` e `C:\Users\User\bin\Slim.exe`: **15.003.648 bytes**, SHA-256 idêntico **`D33AFD02D6A1E45813F10C911507DCBC31E53D14BD992E7A2329CBC71634E7F2`**.
- Binário instalado: `--version` → `slim 0.1.0`; `--headless --fake 'etapa 4 offline'` → `success`; ambos exit 0.
- `rustfmt --edition 2021 --config skip_children=true --check` nos dois fontes → exit 0.
- Revisão documental final: links das etapas existem; `git -c core.safecrlf=false diff --check` global → exit 0; busca por contagem/hash anteriores nos documentos de estado corrente → nenhum resultado. Linhas históricas de gates no tracker foram preservadas. A comparação final dos fontes novamente identificou somente os dois arquivos desta etapa.

### Checklist de RULES.md §4

- [x] Comandos e saídas executados nesta sessão, registrados acima.
- [x] Números contados nesta sessão; benchmark local separado da latência comercial.
- [x] Arquivos/símbolos citados lidos; autoria comparada com as cópias de entrada.
- [x] `cargo test --workspace` verde: 1103 passed, 0 failed, 1 ignored, 0 warnings.
- [x] `.\refresh-slim.ps1 -Test` executado e `OK:` observado; identidade target/PATH conferida.
- [x] Relatório, índices, checkpoint e tracker atualizados; contagens correntes substituídas, gates históricos preservados.
- [x] Limitações declaradas: providers comerciais, console físico, SSE geral/replay opaco, callbacks síncronos e fmt global.

Não aplicáveis: chamada comercial, mudança do contrato visual, teste ConPTY físico, migração de sessão, publicação externa e regeneração do ZIP. O deploy obrigatório é o executável local.

Logs auxiliares: `%TEMP%\slim-harness-etapa4-20260904/{baseline,red,green,usage-red,usage-green,focused,scan-bench,clippy,refresh,fmt-workspace}.log`. A evidência essencial está reproduzida neste registro.

## Continuidade

Preservar o término por wire, a leitura do usage após `finish_reason` Chat, a classificação conservadora de timeout, o acumulador fora do future cancelável e o offset de busca. Não reintroduzir uma segunda cópia do callback de EOF. Qualquer ampliação de SSE/replay opaco deve partir das limitações acima e de fixtures locais específicas; o estado do serviço comercial continua não medido.
