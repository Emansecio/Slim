# Harness Slim — etapa 2: loop de execução e ferramentas

Data: 2026-09-04. Checkout: `D:\Slim`. Estado: concluída. Evidência: código atual, reproduções offline/localhost e gates desta sessão.

## Objetivo e escopo

Reduzir trabalho desnecessário preservando capacidade, autonomia e previsibilidade em todos os providers. A etapa 1 foi lida em `analysis_outputs/HARNESS-SLIM-ETAPA-1.md`, caminho existente no checkout. Foram preservadas suas melhorias e as alterações preexistentes. Não houve provider pago, nova dependência, supervisor, classificador, teto novo, branch, commit ou limpeza do checkout.

Aceitação: corrigir comportamentos reproduzidos, manter as diferenças de protocolo, executar os testes pertinentes e o gate/deploy de `RULES.md`, atualizar o índice e deixar continuidade verificável. Sem redesign visual ou auditoria exaustiva de todos os subsistemas.

## Base e autoria

O patch de entrada e cópias dos fontes ficaram em `%TEMP%\slim-harness-etapa2-20260904`. O diff contra HEAD inclui trabalho anterior e não identifica sozinho esta etapa.

Arquivos alterados nesta etapa:

- `crates/slim-core/src/runtime/mod.rs`: execução, histórico, deduplicação e encerramento.
- `crates/slim-core/src/runtime/governor.rs`: progresso do lote e invalidação após compactação; duas extensões de testes existentes.
- `crates/slim-core/src/runtime/loop_guard.rs`: resposta do usuário invalida falhas anteriores.
- `crates/slim-core/src/tools/mod.rs`: status real da execução shell.
- `crates/slim-core/src/tools/execution.rs`: `--fix` permanece uma barreira serial.
- `crates/slim-core/tests/agent_loop.rs`, `anti_loop.rs`, `tool_contracts.rs`: regressões e atualização das expectativas do comportamento alterado.

Comparação com a entrada: **406 linhas acrescentadas e 269 removidas nesses oito arquivos**. Nos cinco arquivos de implementação, incluindo testes internos já hospedados neles, a redução líquida é de **88 linhas**. Três testes de integração foram acrescentados às fixtures existentes; os demais casos estendem testes existentes. Nenhuma infraestrutura nova.

Uma alteração concorrente em `crates/slim-tui/tests/layout_golden.rs` apareceu depois da cópia inicial. Não foi feita por esta etapa nem revertida. O gate integral valida o checkout observado durante sua execução, incluindo trabalho preexistente.

## Correções implementadas

### E2-1 — falha de processo é falha de ferramenta

**Verificado.** `ToolRegistry::execute_prepared_with_cancellation_and_progress` atribuía sucesso a todo `ExecutedTool`, enquanto `execute_shell` retornava esse tipo também para exit não zero, timeout e cancelamento. A saída textual dizia `exit 7`, mas `ToolResult.success` era verdadeiro. Isso alimentava incorretamente lifecycle, deduplicação e reset do guard.

`ExecutedTool` agora transporta o booleano já conhecido no ponto da execução. Shell exige status de processo bem-sucedido e ausência de timeout/cancelamento. Stdout, stderr e o cabeçalho existente são preservados, inclusive na falha. As demais tools mantêm sua semântica anterior.

RED/GREEN: `shell_result_header_is_humanized_for_model_context` executa `echo failure-output && exit 7`. Antes falhava no assert de `success == false`, exibindo o output completo; depois passou. O caminho de exit zero continua no mesmo teste.

### E2-2 — repetição considera estado e efeitos parciais

**Verificado.** Validações shell estavam excluídas da identidade causal já calculada pelo governor. Agora usam essa identidade, incluindo a revisão observada do workspace. A fixture `failed_validation_can_retry_after_an_observed_external_change` lê um arquivo, executa uma validação que falha, observa uma alteração externa e repete a validação: ambas as falhas chegam ao modelo e ele consegue concluir com o bloqueio conhecido.

Operações classificadas pelo mecanismo existente como `PotentiallyVolatile` invalidam a memória de falhas, inclusive quando falham: um erro não prova ausência de efeitos. A fixture `failed_volatile_shell_keeps_its_side_effects_and_allows_continuation` executa duas vezes um comando que acrescenta uma linha e sai com erro. Antes da correção da fronteira, o runtime retornou `RepeatedFailedTool`; depois retornou `ProviderCompleted`, manteve dois resultados de falha e as duas linhas produzidas. A interação `ask_question` bem-sucedida também invalida falhas anteriores.

Mantido: guard para falhas repetidas classificáveis e fallback para chamadas inválidas; validações sem mudança observada continuam protegidas. Não se afirma que o Slim consiga observar todo estado externo de um comando arbitrário.

### E2-3 — decisões de parada no fim do lote

**Verificado.** O executor já terminava o lote, mas o loop retornava durante o primeiro resultado repetidamente falho. O histórico ficava com a mensagem assistant declarando todas as chamadas e apenas parte dos respectivos resultados. A reprodução ampliou o teste existente para dois reads falhos por lote: antes havia **3 mensagens tool para 4 chamadas executadas**; agora há quatro.

A parada ocorre depois de incorporar todos os resultados, sem clonar o vetor inteiro para o retorno antecipado. Uma mutação ou interação bem-sucedida posterior pode invalidar a falha anterior. O cleanup de encerramento é compartilhado.

No governor, `WouldStop` deixava uma flag permanente mesmo se outra operação do mesmo lote observasse progresso. `finish_turn` agora decide considerando o lote concluído: progresso ou fronteira incerta desfaz a parada e reinicia a contagem de turnos estagnados. O teste `receipts_drive_reuse_and_dependency_changes_without_governor_io` falhou antes e passou depois ao observar uma mudança de arquivo após reads repetidos.

### E2-4 — orçamento preserva as barreiras originais

**Verificado.** `truncate_calls_for_budget` removia chamadas excedentes com `retain`, mas deixava executar operações posteriores. No teste misto, a terceira leitura era omitida e uma escrita posterior acontecia.

Agora se executa somente o prefixo que cabe: a primeira operação suprimida encerra o prefixo. O teste `mixed_batch_budget_preserves_the_execution_prefix` passou de três resultados, incluindo a escrita, para dois reads; o arquivo de saída não é criado. Os valores e a separação dos orçamentos existentes foram preservados.

A execução do prefixo usa o caminho normal de tools/materialização/histórico, eliminando o bloco duplicado de tratamento do orçamento. Não inicia compactação de fundo para um lote que já esgotou o orçamento.

**Verificado adicionalmente:** a allowlist de validações aceitava `cargo clippy --fix --allow-dirty`. A reprodução classificou-o como `Validation` antes da correção. Com `--fix`, a ferramenta permanece volátil e serial. A allowlist existente foi mantida para os demais comandos; não se introduziu um parser de shell ou uma promessa geral de pureza de builds/testes.

### E2-5 — deduplicação não decide progresso pela igualdade do texto

**Verificado.** O aviso de mudar de abordagem/finalizar podia ser produzido apenas por outputs iguais. Agora o aviso depende da anomalia causal de alta confiança. A substituição de conteúdo repetido é apenas economia de representação e só acontece quando o marcador é menor que o conteúdo.

Medição de bytes do caso curto coberto pela fixture: `1: a` com newline ocupa **5 bytes**; o marcador anterior ocupa **68 bytes**. O resultado curto permanece completo. O teste de output longo continua verificando o ponteiro no request seguinte. Isso comprova a redução daquele payload; não houve benchmark de latência, custo comercial ou throughput e não se afirma ganho percentual de desempenho.

Após compactação, os registros de evidência/repetição são invalidados junto com o conjunto de outputs ativos, preservando os stamps e revisões de dependências do governor. Reaquisição de conteúdo removido do contexto volta a fornecer o conteúdo integral sem aviso falso. O teste existente `post_compaction_identical_reread_restores_full_output` detectou a orientação indevida durante o desenvolvimento e passou com a invalidação.

### E2-6 — cancelamento e trabalho de fundo no encerramento

**Verificado.** Cancelar durante a chamada final sem tools podia retornar `TurnLimit`/`ToolLimit`, pois o erro dessa chamada era absorvido e o token não era consultado na saída comum. Agora `Cancelled` prevalece também ali. A fixture `cancellation_during_budget_finalization_reports_cancelled` cancela ao receber o request de finalização localhost e verifica o motivo final e o resultado da ferramenta já executada.

Compactações de fundo prontas são contabilizadas e as pendentes são canceladas antes da chamada final de encerramento; não ficam competindo com uma resposta que não fará novos turnos de trabalho. Foram mantidos cancelamento cooperativo, encerramento de processos, sequência de eventos e contabilização do uso observado.

## Caminhos examinados e mecanismos mantidos

| Área | Evidência atual e decisão |
|---|---|
| Seleção/exposição | `provider_tool_definitions` deriva schemas do registry e modo; omite `code_intel` sem backend; anuncia pergunta somente com rota de interação e em Auto; todo/skill em Auto. Mantida exposição estável, sem seletor por prompt. |
| Lotes/concorrência | Preparação compartilhada deduplica argumentos idênticos; reads contíguos usam `buffer_unordered` com limite existente de 8; aliases reutilizam evidência dentro do segmento; resultados retornam na ordem original. Barreiras, sync LSP após mutação e lifecycle por call_id preservados. |
| Falhas/retries | HTTP não possui retry geral automático neste caminho; overflow de contexto tem uma repetição após compactação, sem saída causal anterior e sem consumir um turno de trabalho. Mantidos timeouts, validação de argumentos/stop e rejeição de replay perigoso em capacidades duráveis. |
| Progresso | Receipts/stamps, revisões e classificação existentes permanecem a base. Todo interno não vira evidência de conclusão. Avisos e guard não foram substituídos por um supervisor. |
| Interação | Pergunta espera resposta real pela rota existente; cancelamento fecha o lifecycle e recusa resposta tardia. Sem reconfirmação extra no harness. |
| Encerramento | Resposta terminal sem tools encerra diretamente; truncamento/filtro bloqueiam execução de tools daquele turno. Budget/no-progress mantêm uma chamada final sem tools. Nenhuma chamada adicional de verificação foi acrescentada. |
| Contexto/caches | Registry compartilhado, paginação, artefatos, redaction e restauração após compactação mantidos. Não foi acrescentado replay local de respostas comerciais. |

Todos os sete braços em `slim-cli/src/headless.rs` chamam `run_agent_loop_with_messages`. As mudanças estão abaixo desse dispatch:

| Provider | Protocolo preservado |
|---|---|
| OpenAI compatible | Chat Completions; deltas e IDs de tools normalizados. |
| OpenAI Codex | Responses; function_call/function_call_output e autenticação próprios. |
| Anthropic | Messages; blocos tool_use/tool_result e encerramento de blocos. |
| OpenCode Go | Chat, Responses ou Messages conforme o modelo atual. |
| ClinePass | Wire Chat por adapter interno; autenticação própria. |
| Command Code | Chat ou Messages conforme o catálogo atual. |
| xAI | Responses por parser existente, com identidade e capacidades próprias. |

Os testes offline de adapters, contratos HTTP e loop foram executados pelo gate integral. Isso não valida autenticação real, comportamento de modelos comerciais ou todos os cruzamentos provider × cenário.

## Verificação desta sessão

- Focados: `cargo test -p slim-core --lib --test agent_loop --test anti_loop --test runtime_abort --test tool_contracts` → **149 passed, 0 failed**, cinco suítes. Depois das últimas mudanças, o gate integral cobriu novamente todos esses testes.
- `cargo clippy --workspace --all-targets -- -D warnings` → exit 0.
- `cargo test --workspace`, executado por `.\refresh-slim.ps1 -Test` → **1099 passed, 0 failed, 1 ignored, 88 suítes, 0 compiler warnings**. Contagem calculada das linhas `test result: ok`, incluindo doc-tests; sem filtros/skip.
- `rustfmt --edition 2021 --config skip_children=true --check` nos oito arquivos alterados → exit 0.
- `cargo fmt --all -- --check` → exit 1 por drift fora dos oito arquivos desta etapa; não foi feita formatação geral do checkout.
- `git diff --check` global e dos arquivos da etapa → exit 0 nesta sessão. A correção concorrente no teste TUI não é autoria desta etapa.
- `.\refresh-slim.ps1 -Test` → exit 0; build release concluído. Saída: `OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 19:16:08)`.
- `target/release/slim.exe` e `C:\Users\User\bin\Slim.exe`: **14.943.232 bytes**, SHA-256 **`0F201CE29C30E59F3DC921CF58978EBF5B4522B13E6EFE5D058CD867620F0E82`**, idênticos na conferência desta sessão.
- Binário instalado: `--version` → `slim 0.1.0`, exit 0; `--headless --fake "etapa 2 offline"` → `success`, exit 0.

Logs auxiliares: `%TEMP%\slim-harness-etapa2-20260904/{focused,clippy,refresh,fmt-workspace,diff-workspace}.log`. As evidências necessárias à próxima etapa estão neste relatório.

### Checklist de RULES.md §4

- [x] Comandos executados e resultados registrados nesta sessão.
- [x] Números calculados nesta sessão; autoria separada do diff contra HEAD.
- [x] Arquivos e símbolos de evidência lidos no código atual.
- [x] Gate integral verde, zero falhas e zero compiler warnings.
- [x] Deploy local com `OK:` e identidade target/PATH conferida.
- [x] Índices/status atualizados; contagens antigas mantidas somente em registros históricos.
- [x] Limitações declaradas: providers reais, console físico, estado externo não observado e fmt global.

Não aplicáveis: migração de protocolo, mudança visual, publicação externa, ZIP de distribuição, nova infraestrutura de testes e chamadas pagas.

## Passagem objetiva para a etapa 3

1. Revalidar checkout e instruções. Os oito arquivos acima são o escopo de código desta etapa; preservar o trabalho de xAI/OAuth/TUI e a etapa 1. Não usar o diff contra HEAD para atribuir autoria.
2. Não reintroduzir sucesso incondicional no shell, filtro de lote que salta operações, retorno antes de completar resultados, orientação baseada só em texto igual ou memória de repetição atravessando compactação.
3. **Limite conhecido:** shell volátil não oferece prova completa de dependências/efeitos. O runtime conserva autonomia nessas fronteiras. Só ampliar sua observação mediante reprodução concreta; não acrescentar classificador ou supervisor genérico. Builds/testes allowlisted também não constituem prova de pureza de todos os scripts executados por eles.
4. **Limite deliberado:** compactação invalida a memória de repetição inteira, mesmo que alguns resultados recentes tenham sido mantidos. Isso privilegia reaquisição legítima; refinamento por conteúdo retido exigiria benefício demonstrado. Sem redefinir compactação agora.
5. **Hipóteses herdadas, não corrigidas aqui:** interpretação duplicada de argumentos/stdin em `main.rs` versus `cli.rs`; cleanup assíncrono do LSP temporário quando seu próprio future é descartado. A etapa 1 não tinha reprodução; esta etapa não as promove a bugs confirmados.
6. Validar futuras alterações com os testes atuais de loop, cancelamento, tools e protocolos. Providers comerciais e ConPTY físico continuam sem evidência live nesta sessão.
