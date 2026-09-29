# Referência detalhada de CLI e runtime

> Conteúdo detalhado anteriormente mantido no README raiz. Contratos podem
> conter registros históricos; confirme o estado vigente no código e no
> [`README principal`](../../README.md).

## Build e deploy do binário (`slim` no PATH)

O comando `slim` do terminal aponta para `C:\Users\User\bin\Slim.exe`, uma
**cópia estática** — ela **não** acompanha o código automaticamente. Depois de
qualquer mudança, rode:

```powershell
.\refresh-slim.ps1          # build release + copia para o PATH + smoke test
.\refresh-slim.ps1 -Test    # idem, rodando cargo test --workspace antes
```

O checkout limita o Cargo a um job em `.cargo/config.toml` para reduzir picos
de commit dos compiladores/linkers. Isso não limita as threads dos testes nem
garante memória suficiente para um processo isolado. Em um host pressionado,
use alvos focados e, se necessário, reduza os símbolos apenas no comando:

```powershell
cargo test -p slim-core --test native_tool_recovery --offline --config=profile.dev.debug=0 --config=profile.test.debug=0
```

O trade-off é menor informação para depuração nessa compilação; os perfis
permanentes e o build release não foram alterados.

Deploy manual equivalente:

```powershell
cargo build --release -p slim-cli
Copy-Item target\release\slim.exe C:\Users\User\bin\Slim.exe -Force
```

**Agentes de código devem rodar `.\refresh-slim.ps1` antes de encerrar
qualquer tarefa que altere código** — ver [AGENTS.md](../../AGENTS.md). O script
também corrige o `RUSTC` da sessão (as variáveis de usuário `RUSTC`/`CARGO`
apontam para um caminho inexistente; detalhes no tracker TUI §8.3).

## Configuração (`slim.toml`)

O Slim lê defaults de dois arquivos TOML, mesclados nessa precedência:

**flag CLI > variável de ambiente > projeto > global > default interno**

| Camada | Caminho |
|---|---|
| Projeto | `./slim.toml` (diretório de trabalho) |
| Global | `%APPDATA%\slim\config\slim.toml` no Windows |

Chaves reconhecidas hoje (desconhecidas são ignoradas):

```toml
model = "gpt-4o-mini"
endpoint = "https://api.openai.com/v1/chat/completions"
effort = "high"   # low | medium | high (label TUI + reasoning_effort no provider)
max_turns = 128
max_mutating_tool_calls = 32
max_read_tool_calls = 96
max_output_tokens = 4096
timeout_secs = 120
max_result_bytes = 16384

[compaction]
enabled = true
background = true
keep_recent_tokens = 20000
summary_max_bytes = 65536
manual_instructions_max_bytes = 4096
```

Se uma resposta atingir o limite de saída, o runtime tenta continuar automaticamente
até duas vezes por execução, preservando respostas a perguntas e resultados já
concluídos. Chamadas de ferramentas cortadas não são executadas. Nessas tentativas,
`max_output_tokens` é o orçamento inicial: o limite enviado pode crescer até 4×
por recuperação, respeitando o teto conhecido do modelo e metade da janela de
contexto (sem metadados, o teto de crescimento é 32.768 tokens). Providers que
omitem o limite continuam omitindo-o. O esforço de raciocínio não muda. As
recuperações de truncamento contam no limite de turnos; se não bastarem, o Slim
informa a interrupção e orienta ajustar o orçamento ou reduzir o próximo passo.
Se o provider rejeitar o aumento por erro no parâmetro de saída, a recuperação
restante volta ao orçamento original, com aviso explícito; esse reenvio não conta
como turno. Retries de transporte (falha recuperável, resposta vazia, `/retry`)
reenviam o turno atual e também não consomem `max_turns`: são limitados pelos
próprios contadores de recuperação.

- Arquivo ausente é normal; **arquivo presente e inválido aborta** com erro
  nomeando o caminho — sem silenciar configuração quebrada.
- As camadas só entram em jogo quando o modo provider está ativo (flag ou env);
  config sozinha não ativa rede.

Tuning de velocidade (env vence o TOML): SLIM_MAX_TURNS, SLIM_MAX_MUTATING_TOOL_CALLS,
SLIM_MAX_READ_TOOL_CALLS, SLIM_MAX_OUTPUT_TOKENS, SLIM_TIMEOUT_SECS, SLIM_MAX_RESULT_BYTES.
Defaults: turns 128, mutating 32/turno, read 96/turno, output pelo catálogo do modelo (4096 se desconhecido; reserva limitada à metade da janela), timeout 120s, result 16 KiB (read completo até 64 KiB).

O timeout de inatividade do stream mede progresso semântico durante toda a
resposta: bytes de keepalive não renovam esse prazo. O prazo total permanece
independente. Na continuação durável, a reconstrução do histórico e a abertura
com validação do prefixo esperado rodam em worker bloqueante, fora do executor
assíncrono; o lock e a validação de concorrência permanecem obrigatórios.
No prefixo durável, anexos cujo payload base64 contém uma credencial registrada
são substituídos por um marcador textual, inclusive na retomada da sessão.

A janela de contexto deve vir do catálogo ou de configuração explícita.
Modelo sem metadados não recebe mais fallback silencioso de 32 mil tokens:
configure `SLIM_CONTEXT_WINDOW_TOKENS` com o limite documentado pelo provider
ou forneça `ProviderRunOptions.context_window_tokens`. Valores positivos
explícitos têm precedência. O limite de saída continua tendo seu próprio default.

O perfil de skills é descoberto uma vez por execução: se não houver entradas
ativas nem diagnósticos, `skill` não é anunciado. Falha de discovery ou skills
inválidas conservam a ferramenta para reportar o erro. A listagem inclui até
cinco avisos limitados a 512 caracteres cada; a TUI também os mostra ao carregar
o workspace, sem repetir na restauração inicial. Nome e descrição vazios são
inválidos. TODO permanece disponível em Auto. Os schemas são reutilizados
por perfil; `code_intel` ainda pode ser anunciado se o trabalho criar um projeto
compatível, evitando esconder uma capacidade que passou a existir.
Reduzir timeout_secs da fail-fast em rede lenta; reduzir max_turns/max_*_tool_calls encurta turnos longos.

No modo headless, `--recover` não lê stdin: recuperação explícita não aceita
prompt e não deve esperar o fechamento de um pipe silencioso.

O adaptador Anthropic preserva texto e thinking presentes em `content_block_start`
antes dos deltas. Chat Completions conserva `reasoning_details` em ordem no
contexto vivo, com o mesmo escopo de conta/endpoint/modelo do reasoning existente.
Esses detalhes opacos não entram no transcript persistido nem no Debug público.

## Admissão e resultados nativos

`write` e `patch` aceitam `then_run: {"command":"cargo","args":["test"]}`.
O comando usa a admissão do `shell`, aceita `timeout_ms`, não aceita `yield_ms`
e reserva duas ações no orçamento. Só executa após a edição bem-sucedida e
aguarda a conclusão. Se o comando falhar, a chamada falha, informa os dois
resultados e preserva a edição. Mutação e validação são registradas separadamente.
Como o comando pode alterar arquivos, uma nova sobrescrita do arquivo editado
exige `expected` ou nova leitura; o LSP relê o estado após o comando.

Falhas de inicialização do LSP incluem o final do stderr do servidor, limitado
a 16 KiB. Após encerrar o processo, Slim aguarda a captura por até dois segundos;
o diagnóstico passa pela redação de valores sensíveis do runtime antes de
entrar nos resultados e eventos de `code_intel`.

A ferramenta `skill` exige `list` booleano e `script` string quando fornecidos;
tipos inválidos, inclusive `null`, são rejeitados antes do despacho. Campos
omitidos mantêm os padrões de listagem e leitura do `SKILL.md`. Chamadas de
script originadas pelo modelo não recebem trust implícito e retornam erro;
o invocador direto só executa script com grant explícito. Essa validação não
acrescenta chamadas ao modelo.

Revisão de 15/09/2026 (checkout, sem deploy):

- `code_intel.path` com tipo inválido falha antes de consultar o backend; omitido,
  `null` e string vazia preservam o significado anterior de ausência de caminho.
- Schemas compartilhados anunciam `query` ou `patterns` em `search` e os campos
  exigidos por ação em `code_intel`. A admissão continua rejeitando combinações
  ambíguas. Nomes desconhecidos recebem uma lista das ferramentas nativas
  registradas e permitidas no modo atual, sem correção automática do nome nem
  alteração de permissões.
- Rejeições estruturais anteriores à execução usam a identidade preparada no
  controle de repetição. Falhas de resolução dependentes do filesystem continuam
  sem ser classificadas como rejeições puramente estruturais. Não há retry
  automático de operações com efeitos iniciados ou resultado incerto.
- Em patch ambíguo, o contexto da primeira ocorrência é apresentado como exemplo,
  com sua linha e pedido de escolha explícita. O exemplo não seleciona a ocorrência
  a editar. Compactação e histórico reconhecem também o marcador antigo.
- O histórico de chamadas deve conter argumentos JSON do tipo objeto antes de
  reconstruir uma sessão ou preparar uma requisição validada, inclusive de
  compactação. Argumentos válidos são preservados; JSON inválido não vira `{}`
  silenciosamente na serialização Anthropic.

Validação local desta revisão: `cargo test --workspace --offline
--config=profile.dev.debug=0 --config=profile.test.debug=0 -- --test-threads=2`,
com `CARGO_BUILD_JOBS=1` e `CARGO_INCREMENTAL=0`: 1.608 testes passaram, zero falhas,
33 ignorados; Doc-tests incluídos. `rustfmt --check` nos arquivos alterados e
`git diff --check` passaram. As primeiras compilações sofreram falta de memória
comprometida; o build serial final concluiu sem alterações globais do ambiente.
Não houve teste com provider real nem deploy. Na fixture de contratos sem backend
semântico, o prompt permaneceu em 1.545 bytes e os schemas passaram de 7.782 para
7.841 bytes. Isto mede bytes serializados, não tokens nem taxa de acerto do modelo.

Revisão de 12/09/2026:

- `shell` com `args` ausente ou `null` executa um script PowerShell. Com um array
  em `args`, executa o programa
  indicado; não interpreta o script automaticamente com um parser POSIX.
  Exemplo direto: `{"command":"git","args":["status","--short"]}`.
  Batch (`.bat`/`.cmd`) mantém as regras de argumentos do seu interpretador.
  No Windows, o modo script fixa UTF-8 sem BOM na entrada e na saída do console
  e em `$OutputEncoding`, inclusive ao encadear processos nativos.
  `timeout_ms` aceita inteiros entre 1 e 3.600.000 (padrão 600.000); fora da faixa a chamada falha
  na admissão, antes de executar.
- `read.lines` é alias de `max_lines`, com a mesma faixa de 1 a 4.096.
  O catálogo enviado ao modelo anuncia somente `max_lines`; o parser mantém o alias.
  Valores iguais nas duas chaves são aceitos; conflito, tipo inválido e campos
  desconhecidos são rejeitados. Exemplo: `{"path":"README.md","offset":10,"lines":12}`.
  Sem limite explícito, a primeira página pode ler até 4.096 linhas dentro do
  orçamento de bytes; páginas posteriores usam 200. Leitura parcial continua
  sem fornecer o recibo completo necessário para overwrite sem `expected`.
- `search.max_entries` é aceito como alias de `max_hits`, com a mesma faixa
  e registro da conversão. Valores iguais nas duas chaves são aceitos;
  conflitos, tipos inválidos e outros campos desconhecidos são rejeitados.
  O schema publicado continua orientando o modelo a usar `max_hits`.
- As formas nativas de patch `edits` e `expected`/`replacement` continuam aceitas,
  de modo exclusivo. Campos desconhecidos também são rejeitados em cada edit.
  Um objeto único em `edits` é normalizado para um array de um elemento antes
  da validação, mantendo a mesma identidade, limites e execução atômica.
  O catálogo anuncia somente `edits`, obrigatório junto de `path`. A orientação
  sobre texto bruto e prefixos aparece uma vez na descrição de `patch`; as
  precondições de overwrite permanecem no campo `write.expected`.
  Esta validação nativa não é aplicada ao payload de ferramentas externas/MCP.
- Valores efetivamente admitidos determinam a identidade da chamada. Aliases,
  cursores normalizados e limites de apresentação não criam identidades
  diferentes para a mesma operação efetiva. Argumentos originais continuam
  correlacionados à chamada, sujeitos à redação de segredos configurada.
- `search.context_lines` aceita inteiro não negativo e aplica no máximo três
  linhas de contexto. Um pedido maior recebe nota com os valores solicitado e
  aplicado, preservada na prévia do modelo. Esta regra é de apresentação;
  não reduz silenciosamente limites de edição ou de execução.
- Fatos de processo (`ToolProcessFinished`, `tool.process.v1`) preservam código
  de saída, timeout, cancelamento, possível captura incompleta e bytes observados
  e descartados. Código zero
  descreve o processo; não certifica todas as operações do script nem a tarefa.
  Ausência de diagnóstico estruturado continua sem classificação automática.
  Para checks nativos, prefira `shell.command` com `args` literais. Em script
  PowerShell, capture `$LASTEXITCODE` imediatamente após o comando nativo,
  antes de filtrar ou imprimir, e encerre com `exit $codigoSalvo`. O retorno do
  último comando do script pode esconder uma falha anterior.
- Em uma execução durável, cada chamada HTTP efetivamente iniciada para um
  provider gera um fato aditivo `provider.call.v1`, com identidade e ordinal
  locais, operação/tentativa,
  provider/modelo, duração, resultado, status HTTP, código normalizado e
  `Retry-After`. Prompts, corpos, headers, tokens e mensagens de erro não são
  persistidos nesse fato; cache hits e cancelamento anterior ao envio não contam
  como chamada. `run.telemetry.v1` também registra o SHA-256 do executável exato
  (ou uma categoria não sensível quando indisponível) e a revisão do build.
  Códigos internos como `transport`, `invalid_response` e `malformed_tool_call`
  preservam sua categoria; códigos desconhecidos continuam sanitizados.
  Se o HTTP terminou com sucesso e a validação do runtime falhou, um fato
  `provider.validation.v1` registra somente a categoria da falha e o ID exato
  de `provider.call.v1`, sem criar outra chamada nem alterar o resultado HTTP.
  Cache replay não gera esses fatos, pois não iniciou uma nova chamada HTTP.
- No loop Auto, `shell` espera até `yield_ms` (padrão 1.000, faixa 0–10.000).
  Se continuar executando, retorna `job_id` e estado `running`; isso confirma
  apenas o início, não o sucesso do comando. `yield_ms: 0` libera imediatamente.
  O prazo `timeout_ms` continua contando desde a execução do processo e não é
  renovado por consultas. Comandos curtos retornam o resultado diretamente.
- `shell_job` recebe `job_id` e `action: status|cancel`. Status mostra tempo,
  última linha e bytes observados, ou o resultado se terminou; cancel solicita
  o encerramento da árvore. `cancelling` não é confirmação de término.
- A conclusão chega ao modelo automaticamente na próxima fronteira de turno.
  Sem trabalho independente, o runtime aguarda localmente e retoma o modelo
  com o resultado, sem chamadas periódicas ao provider. Consultas são úteis
  para inspeção e decisões; não é preciso usar sleep/Get-Process para acompanhar.
  A resposta inicial `running` registra o lançamento, mas o evento terminal
  `ToolFinished` da chamada original só é emitido depois da conclusão real.
  `ToolJobOutput` publica a saída final para TUI/headless e a grava no diário
  como `tool.job_output.v1`, ligada à chamada original sem criar um segundo
  resultado de ferramenta no histórico do provider. O JSONL headless inclui
  essa saída em `tool_job_outputs`; o texto simples mantém a resposta do
  assistente e usa resultados de ferramentas quando ela está vazia.
- Até quatro jobs executam simultaneamente; até 32 resultados ficam retidos
  na execução corrente. IDs expiram ao encerrar a execução ou ao retirar um
  resultado antigo já entregue. Cancelamento, erro ou limites da execução
  interrompem os jobs e aguardam os workers. Não há jobs persistentes após
  reiniciar/retomar uma sessão. Processos não devem ser destacados manualmente.
- Trabalhos independentes podem continuar; alterações em arquivos usados pelo
  comando exigem coordenação pelo agente. Evidências de leitura não são
  reutilizadas pelo cache do harness enquanto há jobs ativos ou no lote que
  inicia shell. Captura e cancelamento continuam usando o executor existente.

- A ferramenta shell mantém uma prévia de até 7 KiB por stream, com início, fim
  e contagem de descarte. Cortes respeitam as fronteiras UTF-8 das partes
  preservadas; bytes parciais entram na contagem de descarte. Quando há
  armazenamento de artefatos, captura até
  8 MiB por stream e arquiva o texto completo capturado se a prévia foi cortada.
  O artefato registra status, bytes descartados, `capture_complete` e
  `utf8_exact`. Acima de 8 MiB por stream ou após interrupção, a captura é
  marcada incompleta; bytes descartados não são recuperáveis. Falha ao arquivar
  aparece no resultado, sem mudar o status do processo. APIs públicas de
  captura bruta preservam seus orçamentos anteriores.
- `artifact_read` lê por ID apenas artefatos registrados na sessão, mesmo que
  a raiz configurada esteja fora do workspace. `offset` é um byte UTF-8 inicial
  (padrão 0), `max_bytes` aceita 4–16.384 (padrão 8.192), e a resposta JSON
  retorna `content`, `next_offset` e `eof`. Cada leitura verifica tamanho,
  digest e tipo do arquivo; IDs ausentes da sessão e offsets no meio de um
  caractere são rejeitados. O conteúdo é texto; bytes não UTF-8 não são
  convertidos silenciosamente pela leitura.
  A indicação operacional de símbolos usa `code_intel action=symbol`.

Integridade de ferramentas e streaming (13/09/2026):

- Sobrescritas e patches preservam a versão deslocada quando ela difere da
  observada ou não pode ser verificada. O resultado informa o caminho de
  recuperação. Falhas de publicação preservam os arquivos recuperáveis,
  informam estado incerto e invalidam evidências anteriores; não há rollback
  nem repetição automática. No Windows, a publicação usa `ReplaceFileW` com
  backup. As pré-condições são de melhor esforço diante de escritores externos,
  sem garantia de comparação-e-troca. Fora do Windows, preservar a versão
  deslocada pode deixar um intervalo sem o pathname entre as duas operações;
  a publicação seguinte não sobrescreve uma terceira edição.
- Falhas na coleta de lotes cancelam e aguardam o trabalho nativo iniciado.
  Se o diário falhar, os resultados coletados continuam no histórico em memória;
  persistência incompleta permanece um erro, sem promessa de ausência de efeitos.
- Falhas de pré-condição de `write` e de correspondência de `patch` apresentam
  causa, efeito e orientação de recuperação sem repetir instruções entre o
  cabeçalho e o contexto. A orientação fixa é
  limitada a 256 bytes, excluindo caminhos e evidência; o resultado completo
  continua sujeito aos orçamentos de saída existentes. O conteúdo necessário à
  correção e os marcadores usados pela compactação são preservados.
  Não há chamada adicional ao modelo para gerar
  essas orientações, nem repetição automática; respostas de sucesso permanecem
  iguais. O limite em bytes não garante redução de tokens por tarefa.
- A recuperação de `write`/`patch` respeita fronteiras UTF-8. O texto canônico
  e o adaptador Chat preservam whitespace recebido em chunks separados,
  incluindo conteúdo de raciocínio. SSE agrega campos `data`
  por evento, com limite de 1 MiB também para o payload agregado.
- Após exclusão externa de um arquivo lido, `write` com `expected` omitido ou
  `null` descarta a observação antiga e pode recriar o arquivo sem sobrescrever
  um arquivo que apareça durante a criação. `expected` explícito continua
  exigindo um arquivo existente.
- Chamadas contendo material sensível registrado são rejeitadas antes da
  execução, sem substituir caminhos, IDs ou argumentos por `[REDACTED]`.
  A TUI sem persistência também conserva o histórico parcial de turnos com erro.

Robustez de admissão para chamadas malformadas (14/09/2026):

- `shell`: `command` contendo um payload JSON de ferramenta é rejeitado na
  admissão com mensagem dirigida. No script form, trechos associados a bash
  (heredoc `<<`, `head -`, `tail -`, `ls -`, `export`, `which`, `chmod`,
  `sed -i`, `awk`, `xargs`, `/dev/null`, `source`) geram nota de admissão na
  saída da chamada; no program form, um `command` de várias palavras junto de
  `args` também gera nota; eval inline (`python -c`, `node -e`, etc.) em scripts
  com quebras de linha gera nota sobre possível risco de quoting. Essas notas
  são heurísticas: strings literais, invocações explícitas de bash e eval
  multilinha podem ser válidos; comandos bem-sucedidos não exigem reescrita.
  O channel stanza do modo Auto informa que o shell é PowerShell, não bash.
  Exit não-zero com stderr vazio recebe nota para interpretar o código e a
  saída conforme o contrato do comando: alguns utilitários usam códigos
  específicos para "sem match"/"diffs existem", mas stderr vazio não implica
  sucesso nem altera o estado de falha da chamada.
- `todo`: uma entrada sem `id` cujo título casa exatamente um item existente é
  aplicada como atualização de status desse item, não como duplicata; sem
  casamento único, a entrada continua sendo adição.
- `patch` sem casamento exato sinaliza `expected` corrompido (U+FFFD) ou com
  texto não-ASCII e instrui cópia verbatim da leitura.
- OpenAI-compatible: deltas identificados não geram uma cópia legada sem
  identidade; chamadas paralelas com IDs distintos conservam sua identidade
  mesmo quando têm nome e argumentos iguais.

Revisão das tools nativas (27/09/2026):

- A recuperação de falhas de `write`/`patch` inclui o arquivo inteiro só até
  12 KiB; acima disso, as bordas somam 8 KiB. O texto cabe inteiro no
  resultado padrão de 16 KiB, em vez de o runtime cortar o arquivo prometido
  como "abaixo".
- `patch` em arquivo uniformemente CRLF converte para CRLF todo LF solto da
  substituição, inclusive quando o `expected` já trazia CRLF.
- `read` não falha a página quando só a linha seguinte tem UTF-8 inválido; o
  erro aparece ao ler aquela linha. `read` em diretório indica `list`, e o
  arquivo ausente indica `list`/`search` antes de `write`.
- `list` em caminho ausente indica o diretório pai ou `search`; em arquivo,
  indica `read`. `patch` em arquivo ausente indica `write` sem `expected`.
- `search` não segue nem abre symlinks (nem junctions no Windows), igual ao
  inventário do workspace. Sem hits e com scan completo, declara
  `[no matches after full scan]`. Arquivos binários, acima de 10 MiB ou com
  UTF-8 inválido contam como não pesquisados e impedem afirmações de ausência
  ou cobertura total.
- `shell`: o rótulo `stderr:` sempre começa uma linha, mesmo quando o stdout
  não termina em quebra de linha. Cada stream fica em até 7 KiB, de modo que
  cabeçalho, os dois streams, marcadores, nota de admissão e referência de
  artefato cabem no resultado padrão de 16 KiB sem segundo corte. O fim do
  stdout, onde executores de teste imprimem o resumo, é preservado.
- Recibos e cabeçalhos de erro de `patch`, `write`, `read` e `search` nomeiam
  caminhos relativos ao workspace, e não mais a forma canônica absoluta
  (`\\?\C:\Users\…`). O conteúdo de arquivo nas mensagens de recuperação não é
  alterado.
- `code_intel`: o texto completo e a projeção sob orçamento saem dos mesmos
  registros e não divergem mais. O corte descarta registros inteiros e mantém
  `[code] (source)` dos diagnósticos, a nota de locais em escopo da definição e
  o aviso de anotações truncadas. Referências mostram `N of total` no
  cabeçalho, o arquivo uma vez por grupo e a continuação na linha
  `more results; pass "offset": …`.
- `patch` bem-sucedido emite `EventKind::ToolEditApplied` (hunks redigidos, com
  linhas no arquivo final, até 400 linhas) entre `ToolOutput` e `ToolFinished`.
  A TUI mostra o diff no corpo expandido (DESIGN §11.4).
- TUI: a row do `search` mostra o texto buscado (§11.4 do DESIGN). Resumos de
  argumentos ficam em até 120 caracteres e marcam com `…` linhas omitidas. A
  nota `[admission: …]` não substitui a prévia do resultado, e `edits` como
  objeto único também recebe `+N -M`.

Validação local de 12/09/2026: `cargo test --workspace --offline --no-fail-fast`
concluiu com **1.521 aprovados, 33 ignorados e zero falhas** (incluídos os quatro
alvos de Doc-tests, todos sem casos). Esse total não inclui a fixture manual
abaixo. `cargo clippy --workspace --all-targets --offline -- -D warnings`
concluiu com exit 0, sem avisos, após corrigir as falhas de integração. `rustfmt --check`
nos arquivos Rust alterados e `git diff --check` também passaram. A revisão
independente confirmou os ajustes de identidade da evidência e dos avisos em lote.

A fixture offline `capture_budget_retained_allocation`, executada separadamente
com `--ignored --nocapture --test-threads=1`, produziu 9 MiB em cada stream e
conferiu bytes de início/fim e descartes. A soma das capacidades dos buffers
finais foi 16.384 bytes no orçamento nativo e 16.777.216 no bruto. Nessa amostra,
o runner levou 1.663,919 ms e 1.675,741 ms, respectivamente; o intervalo de
finalização `wait_complete → handles_closed` levou 0,186 ms e 0,121 ms.
São capacidades retidas finais e uma amostra de tempo, não pico de memória/RSS
nem comprovação de ganho global de velocidade ou tokens. Não houve validação
com provider comercial, LSP externo ou console físico. Deploy não aplicável:
mudança somente no checkout.

## MCP (`[mcp.servers]`)

**Onde configurar** — MCPs não são "instalados" como pacote; são declarados em
TOML em uma destas camadas (o projeto vence o global, campo a campo):

| Escopo | Arquivo |
|---|---|
| Projeto (só este workspace) | `./slim.toml` na raiz do projeto |
| Global (todas as sessões) | `%APPDATA%\slim\config\slim.toml` (ou `SLIM_CONFIG_FILE`) |

`/mcp add` escreve no `slim.toml` do projeto por padrão; `--global` escreve no
arquivo global. `/mcp remove` remove de onde o servidor estiver definido.
`env`/`headers` (segredos) só entram editando o TOML — nunca via `/mcp add`,
para não vazarem no transcript.

A identificação de credenciais usa nomes como `Authorization`, `Cookie`,
`API_KEY` e componentes `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `CREDENTIAL`
ou `SIGNATURE`, sem distinção de caixa. Use esses nomes para material sensível;
valores ordinários de configuração, como `NODE_ENV=production` e flags `1` ou
`true`, não são tratados como credenciais apenas pelo conteúdo.

Servidores MCP (stdio ou HTTP streamable, protocolo `2025-11-25`) em qualquer
camada do `slim.toml`; o projeto vence o global e `env`/`headers` mesclam por chave:

```toml
[mcp.servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { }
enabled = true
timeout_ms = 30000        # 1_000–600_000

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer …" }
```

`command` e `url` são mutuamente exclusivos; `name` aceita `[A-Za-z0-9_-]{1,64}`.
Construir o manager não spawna processo nem abre socket: a conexão só nasce no
primeiro uso real (lazy). O agente recebe uma única meta-tool `mcp` em modo Auto
— `{list:true}` lista servidores sem conectar, `{server,list:true}` lista tools,
`{server,tool,describe:true}` expõe o schema, `{server,tool,arguments:{…}}`
chama. Zero catálogo injetado no prompt; `mcp` conta no bucket mutating e é
serial barrier. Valores de `env`/`headers` entram na redaction do runtime.

### Cancelamento e efeitos externos

Antes de a chamada `tools/call` ser admitida para envio, cancelamento ou deadline
expirado impede o envio desse método. O handshake lazy pode já ter enviado
`initialize` ou `notifications/initialized`; se for interrompido, o resultado
registra o estado de cleanup, inclusive `Unconfirmed` para o transporte HTTP.
Depois que `tools/call` passa à fase de envio, bytes podem já ter sido enviados
mesmo que ainda não haja resposta. O resultado é `OutcomeUncertain`, a execução
não declara rollback e a chamada não é repetida automaticamente.

Quando uma interrupção invalida a conexão, somente a geração afetada é fechada e
seu status passa a `Disconnected` antes do retorno. Uma operação posterior pode
abrir uma nova conexão; ela não repete a chamada incerta. `Confirmed` indica que
o cleanup conseguiu confirmar a parada dos recursos locais que possui, como o
processo stdio e suas threads. `Unconfirmed` informa que essa confirmação não
foi possível. No transporte HTTP, cancelar a espera local não confirma a parada
da requisição remota nem desfaz efeitos já executados pelo servidor.

Na TUI, `/mcp` abre o overlay: nome · transporte · alvo · status (`●` pronto,
`◌` conectando/desconectado, `✕` falha, `○` desabilitado). Teclas: `Enter`
testa, `r` reconecta, `x` desconecta, `d`/`Delete` remove com confirmação,
`R` recarrega config, `Esc` fecha. O polling de 1 s só roda com o overlay
aberto. Comandos: `/mcp add <nome> <comando> [args…] [--global]`,
`/mcp add <nome> --url <url> [--global]`, `/mcp remove|rm <nome>`,
`/mcp reconnect|disconnect <nome>`, `/mcp reload`. Gestão durante run ativo é
recusada ("a run is already active"). Fora da v1: resources/prompts, OAuth,
revisão `2026-07-28`.

## Command Code — DeepSeek V4.1 Flash

O provider nativo `command-code` usa a [Provider API](https://commandcode.ai/docs/provider).
O [plano GOAT](https://commandcode.ai/docs/plans/goat) inclui acesso à API e ao
**DeepSeek V4.1 Flash**, confirmado em 2026-09-10 no catálogo público
`https://api.commandcode.ai/provider/v1/models`.

Na TUI:
1. `/login command-code` — cadastre sua chave do Command Code Studio no campo mascarado.
2. `/models` — selecione **DeepSeek V4.1 Flash** no grupo **Command Code**.
   Também aceita `/model deepseek/deepseek-v4.1-flash` após conectar esse provider.

```powershell
slim --headless --provider command-code --model deepseek/deepseek-v4.1-flash --prompt "hello"
```

O ID exato e padrão desse provider é `deepseek/deepseek-v4.1-flash`, com contexto
catalogado de 1.000.000 tokens e rota `/provider/v1/chat/completions`.
Não é o ID `deepseek-flash` do OpenCode Go. Modelos anteriores são preservados.
Não precisa instalar o CLI Command Code para usar essa integração.

Credencial: `SLIM_API_KEY` > `COMMANDCODE_API_KEY` > `CMD_API_KEY` > chave salva
pelo Slim. Uso consome créditos do seu plano; autenticação e assinatura continuam
sob responsabilidade do Command Code. Nunca coloque chave em `slim.toml`.
`/models` atualiza catálogo em background e salva
`%USERPROFILE%\.slim\command-code-models.json`; IDs oficiais com `:free` também
são aceitos pelo parser, sem invalidar a lista inteira. Sem cache, o catálogo
embutido espelha o snapshot live; configurações de modelo já salvas não são
sobrescritas.

`SLIM_CMD_ZDR=1` (ou `CMD_ZDR=1`) envia `x-cmd-zdr: 1` em toda requisição,
optando pela rotação zero-data-retention documentada — upstreams sem
capacidade ZDR respondem 422 em vez de degradar para retenção.

## OpenCode Go

O provider `opencode-go` usa chave em `SLIM_API_KEY`, depois
`OPENCODE_API_KEY`, depois `auth.json`. Exemplo headless:

```powershell
$env:OPENCODE_API_KEY = "..."
slim --headless --provider opencode-go --model deepseek-flash --prompt "hello"
```

**DeepSeek V4.1 Flash:** no OpenCode Go, o ID oficial é `deepseek-flash`
([endpoints](https://opencode.ai/docs/go/#endpoints), verificado em 2026-09-10).
Na TUI, selecione **DeepSeek V4.1 Flash** em `/models`; `deepseek-v4-flash`
continua sendo a entrada V4 anterior. O alias `deepseek-v4.1-flash` também é aceito.
Se o executável instalado for anterior ao suporte, atualizar só o catálogo não
basta: instale o build atual com `refresh-slim.ps1` e reabra o Slim.

Na TUI, `/login` oferece **OpenCode Go** com campo mascarado e persistência
atômica; ao reiniciar, o provider API-key ativo é restaurado automaticamente.
Credenciais OAuth continuam no fluxo OAuth, com refresh preservado; arquivo de
autenticação inválido produz erro explícito em vez de aparentar logout.
`/models` abre imediatamente e atualiza em background o catálogo
público `https://opencode.ai/zen/go/v1/models`. O último catálogo válido fica
em `%USERPROFILE%\.slim\opencode-go-models.json`; offline, Slim usa cache ou
registro embutido. Somente os 25 modelos documentados são aceitos. Cada modelo
seleciona protocolo, contexto, reasoning e suporte a imagem por metadado
explícito, sem heurística: Chat Completions, Responses ou Anthropic Messages.
No wire Chat Completions, snapshots cumulativos de `usage` de OpenCode Go,
OpenCode Zen e Command Code são consolidados em um único total conservador,
preservando texto e ferramentas. Os demais providers e wires mantêm a validação
terminal existente.
`/logout` remove apenas a chave OpenCode persistida; variáveis de ambiente não
são alteradas.

## OpenCode Zen (free tier)

O provider `opencode-zen` (alias `zen`) usa o gateway principal
`https://opencode.ai/zen/v1`, restrito aos modelos gratuitos. Sem chave, o
tier free responde ao bearer `public` com o header `x-opencode-session`
obrigatório; `OPENCODE_API_KEY`, `SLIM_API_KEY` ou `/login zen` sobrescrevem
com uma chave de conta Zen. Exemplo headless:

```powershell
slim --headless --provider zen --model big-pickle --prompt "hello"
```

`/models` lista o grupo **OpenCode Zen** separado do Go e atualiza em
background o catálogo `https://opencode.ai/zen/v1/models`, intersectado com
os IDs free verificados localmente; o último válido fica em
`%USERPROFILE%\.slim\opencode-zen-models.json`, com o registro embutido como
fallback. O tier free é limitado pelo próprio gateway (rate limit por
sessão).

## Estado de validação e deploy

### Registro de 2026-09-03 (histórico)

1085 testes passados / 0 failed / 1 ignored (ConPTY físico) em 87 suítes —
componentes, contratos e caminhos headless/TUI offline; não comprovam
integração completa do produto (contagem via
`cargo test --workspace -- --skip ordinary_tui_second_turn_sends_prior_user_and_assistant`,
cujo teste filtrado tem expectativa `Explicitly invoked skill` sem implementação em `crates/`):

- `cargo test --workspace` e `refresh-slim.ps1 -Test`: exit code `0`, build
  release, deploy e smoke test concluídos (ver ressalva do teste filtrado acima);
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`;
- `cargo fmt --all -- --check`: acusa somente drift preexistente do rustfmt 1.98
  (trechos novos formatados); `cargo check --workspace` e
  `git diff --check`: exit `0`;
- incluindo proptest, fault injection,
  golden matrix via TestBackend e benchmark long-session com gate de budget;
- fixture offline localhost exercita o turno completo pela API TUI, sem chamada
  a provider live;
- o smoke ConPTY do binário implantado foi executado, mas este host emitiu só o
  probe `ESC[6n`, sem frame; a matriz física completa de terminal, IME, mouse e
  clipboard permanece não observada.

Deploy desse ciclo histórico: `.\refresh-slim.ps1` com `OK:` (build release,
cópia para o PATH e `slim --version` = `slim 0.1.0` com exit `0`). O deploy
vigente está no [registro de deploy](../../release/README.md).

Auditorias persistentes: [WORKFLOW-LOOP-BUGS-SLIM.md](../../analysis_outputs/WORKFLOW-LOOP-BUGS-SLIM.md)
e [TEST-SUITE-OPTIMIZATION.md](../../analysis_outputs/TEST-SUITE-OPTIMIZATION.md)
(índice completo em [analysis_outputs/README.md](../../analysis_outputs/README.md)).

## Status atual de integração

| Área | Estado atual |
|---|---|
| Headless/provider/tools/auth/compaction/usage/artifacts/anti-loop | `Slim --headless` usa caminhos integrados e exercitados offline; OpenAI-compatible/Anthropic SSE, read/list/search/write/patch/shell e filtragem de capabilities funcionam. |
| TUI | `Slim` abre a interface normal mesmo deslogado. `/login` abre seletor OAuth nativo para Claude Pro/Max ou ChatGPT Plus/Pro; Anthropic Messages e Codex Responses usam o mesmo agent loop/tools Slim. Reducer único normativo (`Action → reduce → Effect`), scrollback virtualizado e navegável (pin/live-edge/unseen), paleta estratificada §21.3, ActivityRail por fase/tempo, SessionRail conversacional adaptativa com contexto único e footer Grok-style responsivo (box alinhado + atalhos/metrics), tool blocks tipados agregados, lanes bounded com coalescer no runtime real, command palette Ctrl+P, markdown-light, motion básico com reduced motion e welcome estática com nome, conexão e próxima ação. Fixtures localhost provam OAuth/callback/store, streaming, tool round-trip e cancelamento. PTY E2E físico pendente de console real (teste `#[ignore]`). |
| Cache/HTTP | Replay local de respostas (`ProviderCache`) desligado no produto; transporte HTTP compartilhado reaproveita conexões, enquanto adapters e autenticação continuam isolados por request. |
| Sessões | Writer/recovery/branch e `--session` escrevem JSONL novo; `--resume`/recovery explícitos existem headless e TUI; UX geral de seleção/fork continua limitada. |
| Skills, MCP, subagentes | Tool `skill` lazy (`list` / `name`); roots `.slim/skills`, `.claude/skills`, `.agents/skills` e `.codex/skills` no projeto, `.slim/skills`, `.agents/skills` e `.codex/skills` no perfil (nesta ordem de prioridade; o projeto vence o perfil). Scripts chamados pelo modelo exigem trust explícito e são recusados sem grant; leitura de `SKILL.md` continua disponível. MCP stdio/HTTP lazy via meta-tool `mcp` + overlay `/mcp` (ver seção MCP). Sem child. |
| Todo/Plan/Goal | Tool `todo` no loop Auto; `TodoChanged` abre o dock TUI. Plan/Goal sem UI; Plan headless continua abortando (N4). |
| Release | Determinístico e hash-valid; empacota este checkpoint parcial, não uma v1 completa. |

O restante desta página descreve contratos realmente integrados. Para o plano de
integração pendente, consulte o [índice canônico](../README.md)
e o [plano](../PLANO-IMPLEMENTACAO.md). A fila
curta do ciclo atual (agente completo, harness leve) está em
[próximas etapas](../PROXIMAS-ETAPAS-AGENTE.md).

## Contratos implementados no headless

No Windows, `auth.json` é somente leitura e fail-closed. O arquivo é aberto por
handle `windows-sys` e recebe DACL protegida com allowlist exata do owner atual,
usuário atual, `SYSTEM` e `Administrators`; o teste também cobre caminho Unicode.
Symlink, diretório, reparse point, ACL divergente, JSON/schema inválido ou
versão diferente falham. No headless, arquivo ausente não é criado. A TUI pode
escrever credenciais OAuth tipadas e a chave OpenCode Go no mesmo arquivo por
temp exclusivo, lock, ACL e replace atômico, preservando providers irmãos. Não há detecção mágica de segredos desconhecidos.

O cliente HTTP não segue redirects. O cache é bounded, em memória e por
processo, e está ativo no caminho normal. O transporte Reqwest é compartilhado
para reaproveitar conexões; seu namespace inclui provider, identidade do endpoint e modelo exato,
além de mensagens/tools e conteúdo multimodal canonicalizados. Headers e chaves
ficam fora da chave. Respostas com tool call nunca são cacheadas. SSE exige
evento terminal; bytes/eventos após a terminação ou stream sem terminação são
rejeitados. Os adapters preservam, quando fornecidos, os IDs/index/name de tool
deltas OpenAI e os IDs de `content_block` Anthropic; somente chamadas legadas
sem ID recebem identificador interno. JSON de tool malformado ou incompleto é
rejeitado.

O runtime aplica budgets separados read-only vs mutating por run (`max_read_tool_calls` default 96, `max_mutating_tool_calls` default 32; env `SLIM_MAX_READ_TOOL_CALLS` / `SLIM_MAX_MUTATING_TOOL_CALLS` e chaves `slim.toml`); esgotamento → `tool_limit` (exit 22). O teto de turns por run é 128 (`SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024); esgotamento → `turn_limit` (exit 12) com mensagem `Turn limit reached (N/N)`. Quando o teto por turno corta calls de um batch, a mensagem seguinte ao modelo lista cada call não executada (nome e argumentos abreviados) para que só elas sejam repetidas. A tool `search` respeita `.gitignore`, ignora árvores de build, cap default 200 hits e paginação `offset`/`max_hits`. Emite `ToolStarted`, mantém pares assistant/tool e o prompt raiz através da
compactação no mesmo adapter/modelo. O threshold soft prepara o checkpoint em
background; o hard, `/compact` e uma única recuperação de overflow aplicam o
resumo somente após validação. A última instrução de usuário é preservada
separadamente do sufixo recente, inclusive na retomada de checkpoints; trabalho
encerrado posterior à instrução pode ser resumido. A seleção preserva grupos assistant/tool, o
checkpoint durável usa fingerprint do prefixo, e usage/duração do resumo são
contabilizados mesmo quando uma preparação inválida é descartada. Na aplicação da
compactação, o runtime anexa até 4 KiB de fatos observados: mutações de arquivos,
validações com revisão/época de incerteza e falhas sem sucesso posterior da mesma
chamada. O registro é limitado, informa omissões e mantém o escopo da execução;
não comprova o estado atual após novas ações ou retomada. Com armazenamento de
artefatos, o histórico preserva o texto visível e inclui um índice por linhas para
recuperar mensagens e resultados pelo `read` quando o arquivo está dentro do
workspace; checkpoints anteriores mantêm os
links para arquivos anteriores. Não há chamada adicional de modelo para esses
registros. Cada valor de API key fornecido é redigido exatamente antes
de tool output, follow-up ao provider, renderização e sessão; isso não promete
encontrar qualquer segredo arbitrário que não tenha sido registrado.

Após uma escrita ou patch, o escalonador reavalia as consultas cujas dependências
foram concluídas e usa a concorrência existente, mantendo as barreiras e a
sincronização LSP. A busca multipadrão reserva espaço para padrões ainda não
atendidos, distingue ausência de varredura incompleta e limita a varredura
agregada a 4096 arquivos e aproximadamente 64 MiB (uma leitura de linha pode
cruzar o limite antes da interrupção).

Falhas de walker, abertura, metadados ou leitura deixam a cobertura parcial e
preservam hits válidos já encontrados; padrão sem hit significa ausência não
confirmada, inclusive em busca de um só padrão. O resumo I/O usa contagens
saturantes por categoria e até três exemplos relativos de até 96 bytes, e é
preservado no snapshot e nas páginas por cursor até a apresentação. Se o
orçamento for menor que os marcadores mínimos de incompletude e cursor
(quando houver), a apresentação os mantém, sinaliza `oversized_record=true` e
admite o excesso necessário; `ToolPresentation.complete` continua indicando
entrega dos registros da página, não completude da varredura.

`--image PATH` aceita repetidamente PNG, JPEG/JPG, GIF e WebP; cada entrada deve
ser arquivo regular não-symlink, não vazio e ter no máximo 20 MiB. O conteúdo é
enviado como base64 estrito, sem I/O remoto. Antes do envio pelo adapter Anthropic,
inclusive na compactação, o Slim verifica dimensões pelos cabeçalhos: até
8000×8000 pixels, ou 2000×2000 quando o histórico contém mais de 20 imagens,
e até 10 MiB de base64 por imagem. Cabeçalho ilegível ou limite excedido gera
erro explícito; não há redimensionamento automático nem decodificação de pixels.
`SLIM_CONTEXT_WINDOW_TOKENS` e
`SLIM_MAX_OUTPUT_TOKENS` aceitam inteiros positivos e controlam janela/reserva
de contexto e, quando suportado pelo wire, o teto de saída do provider (padrão:
4096 tokens). No contrato Codex subscription, o valor permanece reserva local e
`max_output_tokens` não é serializado. A apresentação dos resultados usa um
orçamento agregado calculado com o request serializado, a estimativa conservadora
do preflight, os schemas, o histórico e a reserva de saída. Leituras completas
de até 64 KiB continuam possíveis quando cabem. `SLIM_READ_PRESENTATION_BYTES`
configura apenas esse teto entre 1024 e 65536 bytes (padrão 65536); valores
inválidos são rejeitados. Digest, leitura interna e precondições de escrita não
mudam. Páginas menores preservam
registros inteiros e calculam a continuação pelo conteúdo entregue; registros
grandes demais recebem um aviso explícito. Resultados completos são mantidos
separadamente, com artefato quando o armazenamento está configurado.
Nesses resultados, `[artifact id=... size=... path=...]` traz `path` relativo
com `/` quando o arquivo está no workspace; use esse valor em `read.path` e
avance com `read.offset` para recuperar outras páginas. Se o armazenamento
estiver fora do workspace, a referência informa que `read` nativo não o alcança.
`read.path` faz uma leitura comum: não confere o digest indicado no ID do artefato
se o arquivo for alterado depois de armazenado.
O diário mantém o resultado bruto e a projeção vinculada à sua entrada; resume
e branches aplicam essa projeção antes de verificar checkpoints. A projeção
segue a gravação de fatos do diário e é sincronizada pelo próximo append durável.
Branches v2 preparam e sincronizam o cabeçalho e todo o prefixo em arquivo
temporário antes da publicação exclusiva no destino. Falha antes da publicação
não deixa uma sessão filha parcial visível; o pai permanece inalterado.
O branch compactado também prepara o resumo e checkpoint antes dessa publicação;
falha do resumidor mantém o ID do filho livre para retry.

Argumentos grandes de `write` (mais de 4096 bytes) recebem uma projeção temporária
na requisição: `content` vira uma referência com tamanho e SHA-256, somente se
o recibo nativo posterior confirmar sucesso com tamanho e hash correspondentes.
Chamadas incompletas, falhas e `patch` conservam os argumentos. A estimativa de
contexto usa a mesma projeção; histórico canônico, eventos e diário conservam o
conteúdo original. Essa redução de bytes não equivale a economia de custo medida.

A projeção reutiliza um cache por runtime, limitado a 16 entradas e 1 MiB de
texto retido, reiniciado a cada execução. O reaproveitamento exige ID e argumentos
originais idênticos e revalida o recibo no histórico atual. Entradas maiores que
o teto seguem pelo caminho sem cache; o payload enviado permanece igual.

Na estratégia `jev`, o pré-filtro continua seguido pelo resumo do modelo principal.
Pares que não podem liberar os 512 caracteres mínimos ficam fora das consultas;
evidência grande demais ou com blocos estruturados permanece intacta. A preparação
do contexto comum é reutilizada entre estimativa e execução. Até dois lotes são
consultados simultaneamente, mantendo os tetos de tamanho, confiança e tempo.
Uma falha interrompe novos lotes e recolhe o uso confirmado dos que já estavam
em andamento; cancelamento encerra ambos. Nenhuma poda parcial é aplicada.

A decisão econômica do Jev exige margem de 25% e usa a menor tarifa de entrada
ou cache read quando ambas estão configuradas. A previsão usa as quatro últimas
tentativas completas com consumo conhecido, incluindo redução aceita igual a
zero; respostas incompletas, inválidas ou consumo desconhecido não treinam a previsão. Sem observações,
usa a redução líquida máxima estimada. Uma parcela exploratória de 10% desse
máximo permite reavaliar oportunidades maiores após resultados ruins. O histórico
é local ao runtime e reiniciado ao trocar o juiz; preços desconhecidos mantêm
o comportamento anterior, sem alegação de rentabilidade. A compactação obrigatória
continua podendo executar o resumo convencional mesmo quando Jev não compensa.

No headless, text e JSONL expõem `stop`: `provider_completed` (exit `0`),
`turn_limit` e `repeated_failed_tool` (exit `12`) ou `tool_limit` (exit `22`).
O provider/model/IDs informados são preservados; não há troca silenciosa de
provider.

`--verbose` (somente com saída text) acrescenta a timeline humana de tools e o
Usage Ledger v2: input novo, cache write/read, reasoning, cache hit ratio,
latências, tentativas, compactação, ausência de progresso, economia de evidência
duplicada, erro da estimativa e custo por conclusão validada. Argumentos e
outputs não são exibidos; o modo padrão permanece answer-first e
`--verbose --jsonl` é rejeitado.

O JSONL de provider usa `version: 2` e inclui `usage` por requisição, totais da
execução, `costs`, `validation_source` e o qualificador
`compaction_tokens_saved_estimated`, preservando os campos agregados antigos como
projeção. Sem juiz externo, uma resposta final normal, sozinha, não é apresentada
como conclusão validada: o runtime também exige `ValidationGreen`, ausência de
erro terminal e, quando houve mutação, validação posterior à última mutação sem
modificação subsequente. Os preços continuam locais:
`SLIM_INPUT_COST_MICROS_PER_MILLION` e
`SLIM_OUTPUT_COST_MICROS_PER_MILLION`; cache pode ter taxas próprias em
`SLIM_CACHE_WRITE_COST_MICROS_PER_MILLION` e
`SLIM_CACHE_READ_COST_MICROS_PER_MILLION`. Se houver cache sem a taxa
correspondente, o custo exato permanece desconhecido em vez de usar a taxa de
input comum.

Com as quatro taxas acima configuradas, a preparação de compactação em background
compara custo do resumo (input e output) e reconstrução do prefixo com o contexto
mantido. O cálculo favorece conservadoramente o cache existente, usa o maior custo
entre escrita de cache/input para o novo prefixo e exige margem de 25%. Sem taxas
completas, continua o critério em tokens, sem inferir preço. A previsão mantém
dois turnos quando não há histórico de progresso; com tarefas concluídas, usa
turnos observados por tarefa e tarefas abertas, limitada a oito e ao orçamento
restante. Sem tarefas abertas após conclusões, prevê um turno. É uma estimativa,
não uma garantia de duração; compactação necessária no limite hard é preservada.

A estimativa inicial continua sem tokenizer específico (3,5 caracteres/token),
mas requests somente de texto, completos e não servidos pelo response cache a
calibram em memória por provider/modelo com EWMA de caracteres realmente
serializados por token total de input observado. Requests multimodais são
excluídos; cache lido/escrito permanece separado no ledger para não ser vendido
como economia comportamental do agente.

## Comece aqui

O índice canônico está em
[docs/README.md](../README.md).

As decisões normativas estão em
[DECISOES-GRILL-PRE-IMPLEMENTACAO.md](../DECISOES-GRILL-PRE-IMPLEMENTACAO.md)
e o plano histórico em
[PLANO-IMPLEMENTACAO.md](../PLANO-IMPLEMENTACAO.md). As POCs preservadas estão
no [índice histórico](../../poc/README.md); o antigo `POC-RESULTS.md` não está
versionado neste checkout.

## Provider headless e autenticação local

Sem configuração, o CLI usa o provider fake dos testes. A rota HTTP local
suporta `openai-compatible` e `anthropic`; a validação registrada usa somente
fixtures localhost, sem credencial real ou rede externa.

As chaves são resolvidas nesta ordem: `SLIM_API_KEY`, variável específica do
provider (`OPENAI_API_KEY` ou `ANTHROPIC_API_KEY`), `SLIM_AUTH_FILE` e
`%USERPROFILE%\\.slim\\auth.json`. O formato aceito é:

```json
{
  "version": 1,
  "providers": {
    "openai-compatible": { "api_key": "..." },
    "anthropic": { "api_key": "..." }
  }
}
```

O headless não grava `auth.json`. A TUI grava somente credenciais obtidas por
`/login`, preservando entradas API-key existentes. O provider ativo salvo é
restaurado no próximo startup; variáveis de ambiente continuam tendo
precedência e evitam a leitura de um auth file inferior. Access/refresh/code nunca
entram em events ou sessões. `--session PATH` opta por persistir os eventos
JSONL do turno headless; sem essa flag não há sessão headless.

## TUI e OAuth nativo

`Slim` sempre abre a tela normal com composer. Sem credencial, status mostra
`signed out · type /login`; prompt comum é preservado e recebe aviso local.

- `/login`: seletor Anthropic Claude Pro/Max ou OpenAI Codex ChatGPT Plus/Pro;
- autocomplete: digitar `/` em qualquer posição do prompt (início ou meio de
  frase) abre as opções; ↑/↓ navegam, **Tab** completa no draft, **Enter**
  completa e executa, `Esc` fecha;
- `/login anthropic` e `/login codex`: atalhos diretos;
- `/logout`: remove credencial OAuth ativa;
- `/model`: seletor GPT-5.6 Sol/Terra/Luna para Codex;
- `/model sol`, `/model terra`, `/model luna`: aliases diretos;
- Selecionar modelo/esforço/Normal ou Fast altera somente a sessão atual.
  `/model --default` salva a escolha atual na configuração global; o comando
  exige execução ociosa. A precedência CLI > ambiente > projeto > global permanece.
- `Ctrl+Z` desfaz e `Ctrl+Shift+Z` refaz edições do rascunho; digitação contínua
  é agrupada, colagens são atômicas e o histórico é limitado a 64 passos/8 MiB.
  Enviar ou limpar o rascunho descarta o histórico. `Ctrl+Y` continua copiando.
- `Ctrl+←/→` navega por palavras; `Ctrl+Backspace/Delete` apaga a palavra anterior/
  seguinte. Unicode e pontuação seguem os limites de palavra da segmentação
  existente; blocos de colagem são indivisíveis.
- `/retry` solicita uma nova tentativa quando a TUI anuncia conexão pausada
  após esgotar a recuperação automática. A execução fica em memória, com os
  resultados anteriores preservados, sem acrescentar mensagem do usuário.
  Só é elegível a chamada sem texto/raciocínio parcial nem ferramentas emitidas,
  e sem shell jobs em execução.
  Auth, falhas permanentes e limite de turnos não habilitam retry. `Retry-After`
  e cancelamento continuam válidos; pedidos duplicados não ficam enfileirados.
  `Esc` cancela a espera. Fechar/reabrir exige o fluxo durável de recuperação,
  e headless mantém o comportamento terminal anterior. Uma tentativa pode gerar
  cobrança do provedor mesmo sem repetir efeitos locais de ferramentas.
- `Ctrl+V`/`Shift+Insert` e o paste do botão direito: com imagem no clipboard,
  o composer recebe um chip `image · clipboard-N.png` (PNG temporário anexado
  ao próximo prompt); sem imagem, cola texto como antes. `/image PATH` continua
  anexando arquivos locais;
- `Esc`: cancela seletor/login em andamento.

OAuth usa PKCE S256, callback restrito a loopback, validação de `state`, refresh
e browser via `ShellExecuteW` sem shell. Codex usa SSE Responses; WebSocket fica
fora deste checkpoint. Testes são localhost/offline: nenhum login real foi
executado, e política de limites/cobrança pertence aos providers.

Anthropic conserva a validade informada pelo servidor, sem desconto fixo.
Após refresh, a mesma credencial só volta a renovar em background depois de
metade da validade restante, limitada a dez minutos; a janela bloqueante de
30 segundos prevalece. Essa espera fica em memória e não impede a renovação
de outra credencial carregada do store.

## OAuth: seleção e resiliência de credenciais

Cada entrada de provider em `auth.json` pode manter `oauth` e `api_key` ao mesmo
tempo. `preferred_method` é opcional e aceita somente as strings `oauth` e
`api_key`; `null` e qualquer outro valor tornam o arquivo inválido. Quando o
campo está ausente, a leitura conserva a precedência legada: OAuth primeiro,
depois API key. Com uma escolha explícita, somente o método selecionado é usado;
`api_key` não recorre a OAuth nem inicia refresh. Salvar ou ativar um método
seleciona-o sem apagar a credencial alternativa ou as entradas de outros
providers, e um refresh OAuth não altera essa preferência.

`SLIM_API_KEY` e, em seguida, a variável de ambiente específica do provider têm
precedência sobre `auth.json`, inclusive sobre `preferred_method`; isso não
altera a escolha persistida. A versão atual lê arquivos antigos sem o campo e
não os migra apenas por carregá-los. Versões antigas do Slim rejeitam um arquivo
que contenha `preferred_method`, pois o schema delas não aceita campos
desconhecidos.

O supervisor do serviço mantém o trabalho de refresh mesmo se o chamador cancelar
ou abandonar a espera depois do envio. Cancelamento antes da distribuição do
POST impede a requisição; depois que o POST foi distribuído, não pode desfazer
uma requisição já enviada, e o supervisor acompanha sua resposta e persistência.
Um sucessor recebido mas ainda não persistido fica pendente em memória com a
lease de refresh retida. O reconciliador do serviço tenta gravá-lo
automaticamente, sem exigir outra chamada de credencial; o encerramento tenta
uma última reconciliação.

As alterações do arquivo são serializadas pelo lock do auth store. O CAS do
refresh compara, para o provider, OAuth, API key e método escolhido: um login,
logout ou troca de método concorrente vence e impede que um refresh baseado em
estado antigo o sobrescreva. Há uma trava assíncrona por provider dentro do
serviço. No Windows, locks exclusivos de arquivo por auth store e provider
coordenam instâncias entre processos; nas demais plataformas, o código atual
não adquire esses locks de arquivo do sistema operacional, portanto não oferece
a mesma exclusão entre processos.

`OAuthService::shutdown` usa um único prazo global de 35 segundos para aguardar
as tarefas supervisionadas e finalizar a reconciliação, incluindo a aquisição
do lock do auth store. Se o prazo expira, retorna avisos e libera as pendências
em memória ao encerrar o serviço.

Há uma janela inevitável entre o POST e a persistência durável do sucessor. Se o
provider rotacionar o refresh token e o processo morrer antes de gravá-lo, o
sucessor mantido em memória se perde e o lock do processo é liberado. Uma
tentativa posterior pode reenviar a credencial antiga e receber rejeição
explícita, como `invalid_grant`. O fluxo não garante exactly-once nem recuperação
do sucessor que não chegou a ser persistido.
