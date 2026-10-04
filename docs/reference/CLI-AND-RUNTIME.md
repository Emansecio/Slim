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
reserve_tokens = 16384     # gatilho: janela - reserva; também orçamento do resumo
keep_recent_tokens = 20000 # recente mantido literalmente
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
- Servidores MCP (`[mcp]` e `[mcp.servers.<nome>]`) usam as mesmas duas camadas,
  com regras próprias de mesclagem, confiança do projeto e interpolação: ver a
  seção [MCP](#mcp).

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
por perfil; `code_intel` permanece anunciado com um perfil LSP ativo mesmo sem
marcador de projeto ou binário, para permitir diagnóstico de indisponibilidade.

LSP é uma integração nativa e automática: a primeira consulta semântica inicia
rust-analyzer ou TypeScript Language Server, conforme o arquivo. Não exige
ativação, plugin, flag ou TOML. JS/TS cobre .js, .jsx, .mjs, .cjs, .ts, .tsx,
.mts e .cts, com Node.js, TypeScript Language Server e TypeScript clássico com
`lib/tsserver.js` externos já instalados; o Slim não instala essas dependências.
TypeScript nativo sem esse motor fica fora do contrato. A instância JS/TS usa o
workspace autorizado canônico, enquanto Rust conserva a descoberta por Cargo.
Configuração por servidor aceita overrides opcionais de path, args,
initialization_options e settings completos por seção; `path` relativo é
resolvido a partir do workspace, como `cwd` do MCP. Esses campos e
`enabled = true` vindos do `slim.toml` do projeto seguem a mesma confiança dos
servidores MCP do projeto (`--trust-project` ou `/mcp trust`): sem ela, valem os
valores globais e um aviso lista os servidores afetados; na TUI, a confiança
concedida depois vale a partir do próximo início. `enabled = false` do projeto
só restringe e vale sempre. Desativar somente
rust-analyzer mantém JS/TS. Status mostra o ID e o motivo de indisponibilidade;
configurado/disponível não implica processo iniciado: com perfil lançável e
nenhum processo quente, o estado agregado é `starting`. Na busca pelo PATH,
entradas relativas ou vazias são ignoradas, e um launcher TS sem layout npm
suportado não oculta uma instalação válida em entrada posterior. Os defaults JS/TS são
`hostInfo="slim"` e `tsserver.useSyntaxServer="never"`; initialization options
explícitas substituem o objeto completo.

O filtro `server` é aceito somente em `code_intel` symbol e diagnostics. Com
path, o filtro deve corresponder à extensão; sem path, consultas em workspace
misto exigem um perfil explícito. Schema, admissão, identidade preparada e
argumentos tipados conservam o filtro até o backend. Símbolos de workspace JS/TS
abrem uma fonte do snapshot limitado, a mais próxima da raiz, quando disponível,
antes da consulta fria; se ela não puder ser aberta (tamanho, leitura ou UTF-8),
as próximas, até oito, são tentadas. A resposta conserva `completeness=unknown`,
sem certificar o carregamento de todos os projetos.
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

Antes das consultas semânticas, cada instância LSP reconcilia mudanças de fontes
e inputs no disco, inclusive por shell/editor. Inputs Cargo e, para JS/TS,
JSON/JSONC e lockfiles renovam a geração e invalidam continuações antigas; o
processo da geração aposentada é encerrado ao liberar a última lease. A revisão
do workspace é separada por servidor: mudança vista por um perfil não marca
como `stale` nem invalida continuações do outro na mesma raiz. Caminhos que
somem durante a varredura contam como removidos, sem falhar a consulta.
A nota pós-edição usa apenas instâncias quentes e só entra na conversa quando há
algo a fazer: erros nos arquivos editados ou uma verificação tentada que não
concluiu (`no verifiable diagnostics`, limite do lote, falha ao reconciliar o
workspace). Nesses casos informa cobertura, inclusive arquivos não verificados e
seus motivos. Lote limpo, linguagem sem servidor (`language not served`),
servidor inativo (`no active server`) ou arquivo indisponível não geram nota:
cada nota é uma mensagem a mais no contexto e não diria nada acionável. Lotes mistos têm um prazo global de
1,5 s e limite total de 12 arquivos únicos elegíveis, com atendimento concorrente
das instâncias. Publicação ausente ou sem a versão exata não certifica arquivo
limpo. O TLS real publicou sem `version`, que é opcional no protocolo: a consulta
explícita exibe a publicação com `completeness=unknown` e `diagnostic_version=null`;
pós-edição permanece `Unverified`, com motivo `no verifiable diagnostics`, e
não espera o prazo de 1,5 s quando a última publicação do servidor veio sem versão.
A versão local do documento não é inventada como versão da publicação.
Gates reais e versões testadas estão em [release/README.md](../../release/README.md).
Limites e contrato completos em
[atualização do workspace e diagnósticos](../RUST-CLI.md#atualização-do-workspace-e-diagnósticos-pós-edição-30092026).

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
  a editar. A elisão de histórico reconhece também o marcador antigo.
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
  provider/modelo, duração, resultado, status HTTP, código normalizado,
  `Retry-After` e o uso que o provider informou para a chamada:
  `input_tokens`, `output_tokens`, `cache_read_tokens` e `cache_write_tokens`
  (null quando o provider não informou uso ou não separou o cache). Prompts,
  corpos, headers, credenciais e mensagens de erro não são persistidos nesse
  fato; cache hits e cancelamento anterior ao envio não contam como chamada.
  Cada lote de ferramentas executado gera `tool.batch.v1` (chave = ID do lote)
  com `calls`, `call_ids` e `wall_ms`, o tempo de relógio do lote inteiro; a
  duração de cada chamada continua no seu `tool.v1`. Os dois são fatos de
  observabilidade, ignorados pelo replay. `run.telemetry.v1` também registra o SHA-256 do executável exato
  (ou uma categoria não sensível quando indisponível) e a revisão do build.
  Códigos internos como `transport`, `invalid_response` e `malformed_tool_call`
  preservam sua categoria; códigos desconhecidos continuam sanitizados.
  Se o HTTP terminou com sucesso e a validação do runtime falhou, um fato
  `provider.validation.v1` registra somente a categoria da falha e o ID exato
  de `provider.call.v1`, sem criar outra chamada nem alterar o resultado HTTP.
  Cache replay não gera esses fatos, pois não iniciou uma nova chamada HTTP.
- No loop Auto, `shell` espera até `yield_ms` (padrão 1.000, faixa 0–10.000).
  `background: true` ou `yield_ms: 0` inicia sem esperar. `job_id`/`running`
  confirma lançamento; quando omitido, comandos curtos retornam diretamente.
  `timeout_ms` (máximo uma hora) não renova em consultas.
- `shell_job` aceita `list`, `status`, `cancel`, `interrupt`, `output` e `wait`.
  `status`/`cancel` conservam o resultado final do contrato anterior. `list`
  dispensa ID; outras ações exigem `job_id`. `wait` aceita `timeout_ms` de
  0–10.000 (padrão 1.000). Conclusão é automática; não consulte em loop.
  `interrupt` envia SIGINT ao grupo Unix ou Ctrl+Break ao grupo Windows e
  força o encerramento da árvore após `interrupt_grace_ms`. Informa
  `interrupt=graceful|forced`. Sem console Windows associado ou suporte ao
  sinal, encerra a árvore e informa `forced`; não abre janela de console.
- `output` lê stdout/stderr redigidos em ordem observada, por `since` ou
  `offset` (bytes UTF-8), ou `tail` (1–1.000 linhas). `max_bytes` aceita
  1–65.536 (padrão 16.384). Retorna `start_offset`, `next_offset`, `output_bytes`
  e `truncated`; recusa cursor no meio de caractere. UTF-8 inválido vira U+FFFD;
  preserva CRLF. O excedente vai para log na raiz de artefatos existente,
  redigido antes de gravar. Falha de redação/log encerra e informa captura
  incompleta. A prévia final pode ser cortada; `output` lê o log completo.
- Na TUI, terminar a resposta libera o composer; jobs e IDs sobrevivem entre
  prompts e `/compact` da mesma sessão/processo. `/jobs` lista origem, estado,
  duração e exit code. Enter abre saída, ↑/↓ rolam, Home/End e PgUp/PgDn
  navegam páginas de até 64 KiB, `i` interrompe, `x` cancela, `c` copia ID,
  Esc volta. `!& COMANDO` inicia job sem bloquear o composer; `!COMANDO`
  continua foreground. O rodapé mostra contador; conclusão notifica ID,
  exit code e duração. Idle não inicia turno: a conclusão entra como nota
  no próximo prompt. Durante turno ativo, entrega na fronteira de turno.
- Sair, trocar/retomar sessão e `/logout` avisam antes de encerrar e aguardar
  jobs. Job Object protege a árvore Windows em queda; Unix usa grupo e guarda
  de encerramento. Processos não sobrevivem ao reinício. Fatos `shell_job.v1`
  registram metadata pelo journal existente; retomada mostra ativos anteriores
  como `lost`/“perdido”, sem processo vivo. Saída ao vivo de outro processo
  não é restaurada. Resultados já entregues podem expirar por retenção.
- Headless mantém escopo da execução: espera jobs ao final normal e entrega
  antes de terminar. Cancelamento, erro ou limite encerra e aguarda workers;
  text/JSONL conserva motivo e saída terminal. `ToolJobOutput` alimenta
  `tool_job_outputs` no JSONL. Não destaque processos manualmente.

Limites em `slim.toml` (camadas global/projeto existentes):

```toml
[shell_jobs]
max_running = 4            # 1–64
max_retained = 32          # max_running–1024
memory_bytes = 262144      # 4096–16777216; buffers de saída por job
interrupt_grace_ms = 1500  # 50–10000
```

O orçamento divide-se entre final em memória, captura curta e carry necessário
à redação; overhead do processo/estruturas e páginas solicitadas não faz parte
dele. Auto, admissão, sandbox e aprovação existentes continuam valendo.
Chaves/valores inválidos recusam a configuração.

- Trabalhos independentes podem continuar; alterações em arquivos usados pelo
  comando exigem coordenação pelo agente. Evidências de leitura não são
  reutilizadas pelo cache do harness enquanto há jobs ativos ou no lote que
  inicia shell. Captura e cancelamento continuam usando o executor existente.

- Fora do executor de jobs, shell mantém uma prévia de até 7 KiB por stream, com início, fim
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
  correção é preservado; a elisão de histórico reconhece os marcadores de
  recuperação, e a compactação resume esses resultados como qualquer outro
  resultado de tool.
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

<a id="mcp"></a>

## MCP (`[mcp.servers]`)

O Slim é cliente MCP (revisão `2025-11-25`, com negociação para as três
anteriores) por `stdio` e Streamable HTTP. MCPs não são "instalados": são
declarados em TOML, conectam em segundo plano e chegam ao modelo por uma
meta-tool `mcp` (gateway), por tools declaradas diretamente ao provider
(`exposure = "direct"`) e pelo `codemode`. Esta seção descreve o comportamento
do código atual; o que o Slim **não** faz está em [Não suportado](#mcp-nao-suportado).

Fluxo de uma sessão: a configuração é mesclada (global + projeto) → servidores
definidos pelo projeto esperam a confiança do workspace → os valores
`env`/`headers` são resolvidos → os servidores habilitados conectam em segundo
plano → o modelo os alcança por gateway, direct ou codemode. MCP só é anunciado
ao modelo em modo **Auto** e quando há ao menos um servidor habilitado
(`mcp` e `codemode` entram juntos); Plan e Read-only nunca o recebem.

<a id="mcp-configuracao"></a>

### Configuração

**Onde configurar.** Duas camadas TOML; o projeto vence o global, campo a campo
(regras de mesclagem abaixo):

| Escopo | Arquivo |
|---|---|
| Projeto (só este workspace) | `slim.toml` na **raiz do workspace da sessão** (não do diretório do processo: uma sessão retomada usa a raiz gravada nela) |
| Global (todas as sessões) | `%APPDATA%\slim\config\slim.toml` (ou o arquivo de `SLIM_CONFIG_FILE`) |

Arquivo presente e inválido é erro explícito. Config MCP inválida **não** derruba
a sessão nem some em silêncio: o MCP fica desabilitado com aviso (TUI:
notificação "mcp desativado: erro de configuração: …"; headless: aviso em
stderr "MCP disabled: config error: …"). Chaves desconhecidas dentro de um servidor são
ignoradas; um valor inválido de `exposure` é erro de leitura.

```toml
[mcp]
startup_wait_ms = 10000            # padrão 10000; 0 desliga a espera; máximo 120000

[mcp.servers.filesystem]           # stdio
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
cwd = "tools"                      # relativo à raiz do workspace
env = { NODE_ENV = "production" }
description = "Arquivos do projeto"
timeout_ms = 60000                 # padrão 60000; faixa 1000-600000
exposure = "gateway"               # gateway (padrão) | direct | hidden
lazy = false                       # padrão false

[mcp.servers.filesystem.tool_exposure]
"read_*" = "direct"                # nome exato ou glob com `*`
write_file = "hidden"

[mcp.servers.remote]               # Streamable HTTP
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${REMOTE_TOKEN}" }

[mcp.servers.docs]                 # HTTP com OAuth (sem header Authorization)
url = "https://docs.example.com/mcp"
[mcp.servers.docs.oauth]
client_id = "meu-cliente"          # opcional: sem ele, registro dinâmico
client_secret = "${DOCS_SECRET}"   # opcional; aceita interpolação
callback_port = 8765               # opcional: 1-65535
scope = "read"
client_name = "Slim"
auth_server_metadata_url = "https://auth.example.com/.well-known/oauth-authorization-server"
```

**Campos de `[mcp]`:** `startup_wait_ms` (ver [Conexão](#mcp-conexao)).

**Campos de `[mcp.servers.<nome>]`:**

| Campo | Vale para | Padrão / faixa | Observação |
|---|---|---|---|
| `command` | stdio | obrigatório (ou `url`) | não vazio; `~`/`~/` = diretório pessoal |
| `args` | stdio | `[]` | `~/` expande; sem interpolação |
| `env` | stdio | `{}` | interpolação; credenciais viram valores sensíveis |
| `cwd` | stdio | raiz do workspace | relativo à raiz; `~/` expande; precisa ser um diretório existente na conexão |
| `url` | HTTP | obrigatório (ou `command`) | `http://` ou `https://`; sem interpolação |
| `headers` | HTTP | `{}` | interpolação; enviados em todas as requisições |
| `enabled` | ambos | `true` | `false` = desabilitado: nunca lê ambiente, roda comando nem conecta |
| `timeout_ms` | ambos | `60000`; 1000-600000 | por requisição; renovado por `notifications/progress` |
| `description` | ambos | — | não vazia, até 2 KiB; vai ao modelo (awareness, listagens, busca) |
| `exposure` | ambos | `gateway` | `gateway` \| `direct` \| `hidden` |
| `tool_exposure` | ambos | `{}` | tabela `nome-ou-glob` → exposição; chave não vazia |
| `lazy` | ambos | `false` | `true`: conecta só no primeiro uso |
| `oauth` | HTTP | — | tabela abaixo; recusada em stdio |

`command` e `url` são mutuamente exclusivos e um dos dois é obrigatório; `cwd`
só em stdio. O nome do servidor aceita `[A-Za-z0-9_-]{1,64}`; nomes que diferem
só em `-`/`_` colidem (viram o mesmo prefixo `mcp__nome__…`) e são recusados.

**`[mcp.servers.<nome>.oauth]`** (só HTTP): `client_id` (não vazio),
`client_secret` (exige `client_id`; interpolação), `callback_port` (1-65535),
`scope`, `client_name` (padrão `Slim` no registro dinâmico) e
`auth_server_metadata_url` (`https`, ou `http` em `localhost`, `127.0.0.1` ou
`[::1]`). Ver [OAuth](#mcp-oauth).

**Mesclagem entre camadas** (global primeiro, depois o projeto):

- campos escalares (`enabled`, `timeout_ms`, `description`, `exposure`, `lazy`,
  `cwd`, `args`) são substituídos pela camada seguinte;
- `env`, `headers` e `tool_exposure` mesclam por chave; `oauth` mescla por campo;
- uma camada que define `command` descarta `url`, `headers` e `oauth` herdados; uma
  que define `url` descarta `command`, `args`, `env` e `cwd` herdados (as duas
  nunca coexistem no resultado);
- `[mcp] startup_wait_ms` do projeto vence o global;
- um servidor definido ou alterado pelo projeto (qualquer campo além de só
  `enabled = false`) passa a ser *do projeto* e depende da
  [confiança do workspace](#mcp-confianca).

**Credenciais.** Valores resolvidos de `env`/`headers` entram na redaction e no
bloqueio de segredos quando a chave os identifica como credencial (sem
distinção de caixa, `-` = `_`): `Authorization`, `Proxy-Authorization`,
`Cookie`, `Set-Cookie`, nomes que terminam em `API_KEY`/`APIKEY` ou que têm um
componente `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `PASS`, `PASSPHRASE`,
`CREDENTIAL(S)`, `CREDS`, `SIGNATURE` ou `KEY`. Valores ordinários
(`NODE_ENV=production`, `1`, `true`) não são tratados como credencial só pelo
conteúdo. `oauth.client_secret` e os tokens OAuth são sempre sensíveis.

<a id="mcp-interpolacao"></a>

### Interpolação

Semântica do Pi, **somente** nos valores de `env`, `headers` e
`oauth.client_secret` (nunca em `command`, `args`, `cwd`, `url`):

- `$VAR` e `${VAR}` inserem a variável de ambiente (nome `[A-Za-z_][A-Za-z0-9_]*`);
  variável ausente **ou vazia** é erro; `${…}` com nome inválido e `$` solto ficam
  literais;
- `$$` produz `$` literal e `$!` produz `!` literal;
- um valor que **começa** com `!` executa o resto como comando de shell no
  diretório do workspace: PowerShell (`pwsh`, senão `powershell`) no Windows,
  `sh -c` nos demais; limite de 10 s e 64 KiB por stream; o stdout aparado é o
  valor (vazio é erro). O comando precisa ser o valor inteiro: nada dentro dele
  é interpolado.

Variável ausente ou comando que falha **bloqueia só aquele servidor** (status
`failed`, com o motivo, que cita a variável ou o tipo de falha — nunca o valor
nem a linha de comando). A resolução ocorre ao montar a configuração (abertura
da sessão, `/mcp reload`, mudança de confiança), não a cada conexão; um comando
lento atrasa esse passo em até 10 s por valor. Servidor desabilitado ou de
projeto sem confiança não lê variável nem roda comando.

<a id="mcp-confianca"></a>

### Confiança do projeto

Um `slim.toml` de projeto pode nomear qualquer executável, então os servidores
definidos (ou alterados, exceto por `enabled = false`) por ele **não iniciam**
até o workspace ser confiável; servidores do arquivo global são sempre
confiáveis. Enquanto não confiável o servidor fica listado como
`untrusted` ("projeto sem confiança" na TUI), sem ler variável nem executar
`!comando`, e uma chamada do modelo a ele é recusada com a instrução de confiar.

| Quem | Como |
|---|---|
| TUI | aviso na abertura; `/mcp trust` confia, `/mcp untrust` grava "nunca" (some o aviso e os servidores seguem desligados) |
| Headless | aviso em stderr; `--trust-project` confia **só naquela execução** (nunca grava) |
| TUI com a flag | `slim --trust-project` vale para a sessão inteira, sem gravar |
| CLI | `slim mcp trust` / `slim mcp untrust` no projeto atual |

A decisão fica em `mcp-trust.json`, na mesma pasta do `slim.toml` global
(`SLIM_MCP_TRUST_FILE` a substitui), por caminho canônico do workspace, num
arquivo restrito ao usuário (mesma disciplina de `auth.json`; os helpers de
arquivo seguro só funcionam no Windows, então fora dele só `--trust-project`
funciona). Ficar fora do workspace impede que um projeto se autoconfie. Falha ao
ler o arquivo é tratada como "não confiável" e reportada; um arquivo de versão
estranha ou corrompido nunca é sobrescrito.

<a id="mcp-importar"></a>

### Importar de outros clientes

`slim mcp import <arquivo> [--global|--project] [--force]` converte o JSON de
outro cliente em entradas `slim.toml`. Formatos aceitos (o primeiro que
casar): `{"mcpServers": {…}}` (Claude Desktop, Claude Code `.mcp.json`, Cursor,
Pi `mcp.json`), `{"servers": {…}}` (VS Code `mcp.json`) e
`{"mcp": {"servers": {…}}}` (settings do VS Code); comentários e vírgulas finais
são aceitos. Mapeamento:

| Origem | Slim |
|---|---|
| `type`/`transport`: `stdio`, `local` / `http`, `streamable-http`, `remote` (ou inferido de `command`/`url`) | stdio / HTTP |
| `serverUrl` | `url` |
| `command`, `args`, `env`, `cwd`, `headers` | idem |
| `${env:X}` e `${X}` em `env`/`headers`/`oauth.clientSecret` | `${X}`; um `$` literal vira `$$`; um valor começando com `!` vira `$!…` |
| `timeout` (segundos, 1-600) | `timeout_ms` |
| `enabled` / `disabled` | `enabled` |
| `description`, `exposure` (`gateway`, `codemode`, `codemode-deferred`→gateway; `direct`; `hidden`; `deferred`→gateway com aviso), `toolExposure` | idem |
| `oauth.{clientId,clientSecret,callbackPort,scope,clientName,authServerMetadataUrl}` (só HTTP) | `oauth.*` |

Servidores que o Slim não pode representar são **pulados com o motivo**: SSE
legado (`type: "sse"`), `${input:…}` ou qualquer `${…}` em `command`, `args`,
`cwd` ou `url`, nome inválido ou colidente, entrada que falha a validação.
Campos sem equivalente são descartados com aviso. Nada é sobrescrito sem
`--force` (com ele a entrada é substituída por inteiro); valores literais do
arquivo de origem (por exemplo um token em `env`) são copiados como estão para o
TOML, então prefira referências `${VAR}`.

<a id="mcp-comandos"></a>

### Gerenciar servidores: `slim mcp` e `/mcp`

**CLI** (funciona sem sessão TUI nem credenciais de provedor; o workspace é o
diretório atual; sai com 0 em sucesso e 1 em qualquer falha). Cada escrita
**reserializa** o TOML do arquivo alterado: as demais tabelas e valores são
mantidos, mas comentários, ordem das chaves e formatação se perdem (vale para
`add`, `remove`, `enable`, `disable`, `import` e para as ações equivalentes do
`/mcp`):

```text
slim mcp add <nome> [--project|--global] [--timeout-ms N] [--exposure E]
    [--description T] [--lazy] [--env K=V]... [--cwd DIR]
    -- <comando> [args...]
slim mcp add <nome> [--project|--global] [--timeout-ms N] [--exposure E]
    [--description T] [--lazy] --url <url> [--header K=V]...
    [--bearer-token-env-var NOME] [--oauth-client-id ID]
    [--oauth-client-secret S] [--oauth-callback-port P]
    [--oauth-scope S] [--oauth-client-name N]
slim mcp remove <nome> [--project|--global]
slim mcp enable <nome>
slim mcp disable <nome>
slim mcp list [--json] [--timeout SEGUNDOS]
slim mcp login <nome> [--timeout SEGUNDOS] [--no-browser]
slim mcp logout <nome>
slim mcp import <arquivo> [--project|--global] [--force]
slim mcp trust
slim mcp untrust
```

- `add` e `import` gravam no `slim.toml` **do projeto** por padrão (`--project`);
  `--global` grava no global. `--env` e `--cwd` valem só para stdio;
  `--header`, `--bearer-token-env-var` e `--oauth-*` só para HTTP (o uso trocado
  é recusado antes de gravar). `--bearer-token-env-var NOME` grava
  `Authorization = "Bearer ${NOME}"` e conflita com um `--header Authorization`.
  `--timeout-ms` aceita 1000-600000; `--exposure` é `gateway|direct|hidden`;
  `--lazy` conecta só no primeiro uso. O comando pode vir sem `--`: a partir da
  primeira palavra do comando, tudo é repassado a ele. Também há `--opção=valor`,
  os aliases `rm`/`ls` e `slim mcp help`.
- `remove` sem flag apaga do arquivo do projeto e, se não achar, do global; com
  `--project` ou `--global` só toca naquele arquivo (e avisa se o servidor
  continua definido na outra camada). `enable`/`disable` gravam só `enabled` no
  arquivo que define o servidor (projeto primeiro) e são idempotentes.

- `add` mescla numa entrada existente em vez de substituí-la (mantém
  `env`/`headers`/`timeout_ms`/`enabled`/`oauth`… que a chamada não muda) e
  escolher `command` remove as chaves HTTP (e vice-versa); a saída diz "Added"
  ou "Updated". `--env`/`--header`/`--oauth-client-secret` gravam o valor como
  digitado, em texto no TOML e no histórico do shell: para segredos use uma
  referência (`--env TOKEN='${MEU_TOKEN}'`) ou `--bearer-token-env-var`; o
  comando avisa em stderr, sem repetir o valor, quando um valor com cara de
  credencial não é uma interpolação (`$VAR`, `${VAR}`, `!comando`).
- `list` **conecta** em paralelo cada servidor habilitado e confiável
  (inclusive `lazy`), até `--timeout` segundos cada (padrão 15), e desconecta ao
  final; servidores de projeto sem confiança são só reportados, nunca iniciados.
  Mostra transporte, escopo, exposição, destino (URL sem credenciais), tools
  (com exceções de exposição por tool), contagem de recursos e modelos e a
  identidade do servidor; entradas inválidas aparecem como `config error: …`.
  Sai com código 1 se houver entrada inválida ou servidor **habilitado** que não
  esteja `connected` (inclui `needs-auth`); `untrusted` e `disabled` não contam.
  Se um servidor stdio morre durante o handshake, o motivo é "the server closed
  the connection before it answered", sem o stderr dele.
- `list --json` imprime `{workspace, servers, errors, notes}` (chaves em ordem
  alfabética). Cada item de `servers` tem `name`, `scope` (`project|global`),
  `source` (arquivo), `enabled`, `transport` (`stdio|http`), `target`,
  `exposure`, `lazy`, `status` e `tools` (nomes), mais, quando existem,
  `description`, `tool_exposure` (só tools com exposição diferente da do
  servidor), `resources`, `resource_templates`, `resources_error`, `server`
  (`name`, `version`, `protocol_version`) e `error`. `errors` são as entradas
  inválidas e `notes` avisos gerais. `status` é um de `connected`, `disabled`,
  `untrusted`, `needs-auth`, `failed` ou `invalid` (variável não resolvida ou
  `!comando` que falha); os rótulos `ready`, `connecting` e `disconnected` são do
  gateway e do TUI, não da CLI. Erros e destinos saem redigidos.
- `login` só vale para servidor HTTP habilitado, sem header `Authorization` (e
  confiável, se for de projeto). Conecta antes e, se não precisa de login, diz
  isso e sai com 0. Senão imprime a URL de autorização, abre o navegador (a
  menos que `--no-browser`, útil em SSH) e espera o callback em
  `127.0.0.1:<porta>/callback` por `--timeout` segundos (padrão 300); em um TTY
  também aceita colar a URL de redirecionamento. Sai com 1 em timeout,
  cancelamento ou falha. `logout` apaga as credenciais guardadas (por nome e
  URL), sem conectar, e funciona com servidor desativado; sai com 1 se o servidor
  não existe ou não usa OAuth. É o mesmo fluxo OAuth do TUI ([OAuth](#mcp-oauth)).
- `import` aceita até 4 MiB de JSON `mcpServers`/`servers` (comentários e
  vírgulas finais são tolerados), mantém entradas existentes (relatadas como
  "Kept existing") a menos que `--force`, pula nomes que só diferem de um
  existente por `-` versus `_` e sai com 1 só se o arquivo é ilegível ou nada foi
  importado porque todos foram rejeitados.
- `trust`/`untrust` agem sobre o workspace do diretório atual: `trust` lista os
  servidores do `slim.toml` do projeto que passam a poder iniciar (com as suas
  permissões) e não pede confirmação; `untrust` grava "nunca", igual a
  `/mcp untrust`.
- `slim --help` lista `slim mcp`. Só o primeiro argumento `mcp` roteia para o
  subcomando: `slim --headless mcp` continua sendo um prompt.

**TUI.** `/mcp` abre o overlay (nome · transporte · alvo · estado) com os estados
`●` pronto (com a contagem de tools), `◌` conectando / desconectado, `!`
projeto sem confiança / requer login, `✕` falhou (com o motivo), `○`
desativado; mostra também contagem de recursos e a exposição do servidor (só as
contagens conhecidas: as de recursos aparecem depois de `Enter`/`r` ou de o
modelo listar recursos).
Teclas: `Enter` testa (conecta e conta as tools), `r` reconecta, `x`
desconecta, `a` ativa/desativa, `l` entra (OAuth) e `o` sai (com confirmação),
só em servidor HTTP, `t` confia no projeto (só em servidor sem confiança),
`d`/`Delete` remove com confirmação, `R` recarrega a config, `Esc` fecha. A URL
de autorização do login aparece num painel próprio (`Ctrl+Y` copia a URL
inteira; um campo aceita a URL de redirecionamento colada; `Enter` envia, `Esc`
ou `Ctrl+C` cancela). O polling de 1 s só roda com o overlay aberto. O mapa completo de
teclas e o layout seguem o contrato visual em
[`DESIGN-SLIM-TUI.md`](../DESIGN-SLIM-TUI.md).

Subcomandos (argumentos aceitam aspas `"…"` e `'…'`; no Windows a barra
invertida só escapa antes de aspas duplas):

```text
/mcp add <nome> <comando> [args…] [--global]
/mcp add <nome> --url <url> [--global]
/mcp remove|rm <nome>        /mcp reconnect|disconnect <nome>
/mcp login <nome> [url-de-redirecionamento]     /mcp logout <nome>
/mcp enable <nome>           /mcp disable <nome>
/mcp trust [nome]            /mcp untrust [nome]
/mcp reload
```

`/mcp add` escreve no `slim.toml` do projeto (da raiz do workspace) por padrão,
`--global` no global, sempre por mesclagem; `env`/`headers` (segredos) só
entram editando o TOML ou por `slim mcp add`, nunca por `/mcp add`, para não
vazarem no transcript. `/mcp remove` apaga do primeiro arquivo que define o
servidor (projeto, depois global): se um servidor do projeto sobrescrevia um
global, o global volta a valer. `enable`/`disable` escrevem `enabled` no arquivo
que define o servidor. Gestão durante um run ativo é recusada ("a run is already
active"). Na abertura, **uma** notificação lista os servidores que não subiram
(falhos, "requer login", sem confiança) e aponta `/mcp`.

<a id="mcp-protocolo"></a>

### Transportes e protocolo

**stdio.** O comando é resolvido pelo `PATH`, roda em `cwd` (raiz do workspace por
padrão) com o **ambiente do processo mais** `env` (o ambiente é herdado, não
limpo). Mensagens são JSON por linha; linhas de stdout que não são JSON são tratadas
como ruído e não derrubam o transporte (servidores que poluem o stdout seguem
funcionando).
O stderr fica num rabo de 16 KiB anexado aos erros de transporte. Fechar a
conexão mata a árvore de processos (Job Object `KILL_ON_JOB_CLOSE` no Windows,
grupo de processos nos demais).

**Streamable HTTP.** `POST` por requisição com
`Accept: application/json, text/event-stream`; a resposta é JSON ou um stream
SSE. Headers configurados, `MCP-Protocol-Version` (versão negociada) e
`mcp-session-id` (quando o servidor emite; até 1024 bytes) vão em todas as
requisições. Redirects são proibidos (um redirect reenviaria headers secretos a
outro host). Depois de `notifications/initialized`:

- **Stream `GET`**: o Slim abre um `GET` SSE para o servidor falar sozinho
  (`tools/list_changed`, `resources/list_changed`, logs, progresso, pedidos
  `roots/list`/`ping`). `405` = o servidor não oferece (sem nova tentativa); um
  4xx permanente encerra só o stream. Se cai, reconecta com backoff
  exponencial (1 s dobrando até 30 s; 5 falhas seguidas; o `retry:` do servidor
  substitui o atraso, limitado a 100 ms-30 s) e `Last-Event-ID`; o mesmo vale
  para uma resposta SSE de requisição interrompida (retomada por `GET`, nunca
  reenvio do `POST`).
- **Fechar**: com sessão, o Slim envia `DELETE` (limite de 1 s; aguardado no
  desligamento, em segundo plano nos demais casos).
- **Sessão expirada**: `404` numa requisição que levava `mcp-session-id`. A
  conexão abre sessão nova (repetindo o `initialize` original) e repete **uma
  vez** só as requisições idempotentes (`tools/list`, `resources/list`,
  `resources/read`, `resources/templates/list`). `tools/call` nunca é repetida:
  o resultado é `OutcomeUncertain` e o uso seguinte reconecta.
- **Retentativas de conexão**: falhas transitórias (erro de rede, `408`, `429`,
  `5xx` exceto `501`) repetem 2 vezes, após 250 ms e 1 s; stdio não repete.

**Handshake.** O `initialize` pede `2025-11-25`, declara `roots: {}` e
`clientInfo` `slim`, e aceita a versão escolhida pelo servidor se for
`2025-11-25`, `2025-06-18`, `2025-03-26` ou `2024-11-05` (outra falha a conexão
com a lista de suportadas; um `protocolVersion` ausente é tolerado). A versão
negociada vai em `MCP-Protocol-Version`. `serverInfo`, `capabilities` e
`instructions` (texto do servidor, até 4 KiB) ficam disponíveis enquanto a
conexão vive. Servidor sem a capability `tools` não recebe `tools/list`; o
catálogo lê até 8 páginas e 256 tools por servidor. Uma resposta de erro a
`notifications/initialized` falha a conexão HTTP.

**Pedidos do servidor ao cliente.** `ping` recebe `{}`; `roots/list` recebe a raiz
do workspace como URI `file://`; qualquer outro (sampling, elicitation, …)
recebe `-32601`.

**Cancelamento, timeout e progresso.** Ao cancelar ou estourar o timeout de uma
requisição já enviada (exceto `initialize`), o Slim envia
`notifications/cancelled` com o `requestId` e um motivo (melhor esforço: em HTTP
um `POST` de até 500 ms; em stdio escreve na pipe e dá 100 ms de graça antes de
derrubar o processo) e o resultado continua `OutcomeUncertain`, nunca
reexecutado. Toda `tools/call` leva `_meta.progressToken`; cada
`notifications/progress` do servidor (pelo `POST` ou pelo stream `GET`) renova o
timeout da chamada e aparece como evento de progresso da tool (texto de até 512
caracteres, redigido, no máximo um evento a cada 100 ms, fila de 16). Não há teto
total: um servidor que sempre manda progresso mantém a chamada aberta até o
usuário cancelar (ou, no codemode, até o prazo da célula).

**Log do servidor.** `notifications/message` é gravado em
`logs/mcp.log` na pasta do `slim.toml` global
(`%APPDATA%\slim\config\logs\mcp.log`), uma linha
`<ISO> [servidor] nível logger: texto`, com redaction dos segredos conhecidos,
entradas de até 8 KiB, sem caracteres de controle (quebras de linha são
indentadas, então uma entrada não forja outra) e rotação para `mcp.log.1` acima
de 5 MB. Falha ao gravar o log é ignorada. Não há `logging/setLevel`.
`notifications/resources/list_changed` invalida os caches de recursos e
`notifications/tools/list_changed` marca o catálogo para recarga no próximo uso.

<a id="mcp-conexao"></a>

### Conexão e estados

Construir o manager não abre processo nem socket. Na abertura da TUI (e a cada
execução headless) os servidores **habilitados, confiáveis, não `lazy` e não
totalmente `hidden`** conectam em segundo plano, sem bloquear. A primeira
requisição ao modelo espera apenas os servidores de exposição `direct` ainda
conectando, até `[mcp] startup_wait_ms` (padrão 10000, máximo 120000, `0`
desliga); se estourar, segue com o aviso "MCP servers still connecting after Ns,
continuing without them: …" (TUI: notificação; headless: stderr). A espera é
gasta uma vez por sessão (volta a valer quando um servidor `direct` novo começa
a conectar). Uma chamada do gateway espera só o servidor que nomeia,
reaproveitando a conexão em andamento (um único `initialize`). Servidores `lazy`
conectam no primeiro uso; um que falhou no início fica `failed` e é tentado de
novo quando usado. `/mcp reload`, `/mcp add` e a mudança de confiança iniciam a
conexão dos novos; reconciliar reconecta o servidor cujo spec mudou (inclusive
um valor `!comando` que passou a resolver diferente). O encerramento da sessão
cancela conexões em curso.

Estados de um servidor no gateway (`mcp {list:true}`) e no `/mcp`: `disabled`,
`untrusted`, `disconnected` (ainda não conectou), `connecting`, `ready`,
`failed` (com motivo, redigido), `needs-auth`. O `slim mcp list` (texto e
`--json`) conecta de fato e usa outro vocabulário: `connected`, `disabled`,
`untrusted`, `needs-auth`, `failed` e `invalid`.

<a id="mcp-oauth"></a>

### OAuth (servidores HTTP)

Um servidor `url` **sem** header `Authorization` próprio autentica por OAuth
(MCP authorization `2025-11-25`) quando responde `401`; configurar um
`Authorization` desliga o OAuth daquele servidor. O Slim **nunca abre o
navegador sozinho** ao conectar: usa o token guardado, tenta renová-lo uma vez
após `401` e, sem credencial válida, o servidor fica `needs-auth` ("requer
login" na TUI; a chamada do modelo explica o motivo).

**Entrar:** `/mcp login <servidor>` (TUI) ou `slim mcp login <servidor>
[--timeout s]` (CLI; padrão 5 min). O fluxo:

1. descobre o servidor de autorização: RFC 9728 (`resource_metadata` do
   `WWW-Authenticate` ou `/.well-known/oauth-protected-resource`, com variantes
   por caminho), depois RFC 8414/OIDC com fallbacks por caminho; o `issuer`
   precisa coincidir; `oauth.auth_server_metadata_url` substitui a descoberta;
   sem metadata nenhum, usa `/authorize`, `/token` e `/register` na origem;
2. usa o cliente de `oauth.client_id` (+ `client_secret`;
   `client_secret_basic`/`client_secret_post`/`none` conforme o servidor) ou
   registra um por registro dinâmico (RFC 7591; só é gravado ao concluir);
3. código de autorização com **PKCE S256** (falha se o servidor lista métodos e
   não inclui S256), `state`, checagem de `iss` (RFC 9207) e `resource` (RFC 8707);
   o escopo é a soma de `oauth.scope`, do que o `403` pediu e do `WWW-Authenticate`
   (senão, `scopes_supported`);
4. escuta em `127.0.0.1:<porta>/callback` (`oauth.callback_port`; senão a porta
   do registro anterior; senão uma efêmera), abre o navegador e espera 5 min.
   Só aceita pares loopback, `GET /callback` com o `state` certo; requisições
   estranhas não abortam o login. Sem alcance ao navegador (SSH), cole a URL de
   redirecionamento: `/mcp login <servidor> <url>` (TUI) ou na entrada do
   `slim mcp login`. `/mcp login` de novo reinicia um login em andamento; durante
   um run ativo é recusado.

**Sair:** `/mcp logout <servidor>` / `slim mcp logout <servidor>` apaga as
credenciais guardadas.

**Armazenamento.** `mcp-auth.json` na pasta do `slim.toml` global
(`SLIM_MCP_AUTH_FILE` a substitui), arquivo restrito ao usuário, chave por nome
**e** URL do servidor (estado de outra URL é ignorado). Guarda tokens e o cliente
registrado dinamicamente; o `client_secret` configurado nunca é gravado.

**Renovação.** O token é renovado 30 s antes de expirar e após `401`; uma única
renovação por processo e um lock entre processos (obsoleto após 20 s) evitam
perder um refresh token rotativo; tokens trocados por outro processo são
reaproveitados. `expires_in: 0` é tratado como "não informado". Um `403`
`insufficient_scope` não é repetido: o servidor vira `needs-auth` e o próximo
login pede os escopos somados. Uma `tools/call` rejeitada com `401` não foi
executada, então repeti-la após renovar não viola a regra de não reexecução.

**Limites.** Todo endpoint (descoberta, autorização, token, registro) precisa ser
`https`; `http` só em loopback e apenas quando o próprio servidor MCP é loopback
(ou a URL veio de `oauth.auth_server_metadata_url`); o token só vai a servidores
MCP `https` ou loopback. Requisições OAuth: 15 s, resposta de até 256 KiB, sem
redirects, URLs com credenciais recusadas, erros sem URL nem segredo. Tokens e
segredos de cliente são valores sensíveis (redaction e bloqueio modelo→servidor) e
nunca vão ao log.

<a id="mcp-exposicao"></a>

### Exposição, busca e awareness

`exposure` (por servidor) e `tool_exposure` (por tool) escolhem como o modelo
alcança cada tool. Em `tool_exposure` vale o nome exato; senão o glob com `*`
mais específico (mais caracteres literais; empate, o primeiro por nome); senão o
`exposure` do servidor.

- `gateway` (padrão): pela meta-tool `mcp` e pelo `codemode`.
- `direct`: a tool é declarada ao provider como `mcp__<servidor>__<tool>`.
  Caracteres fora de `[A-Za-z0-9_]` viram `_` e o nome tem no máximo 64
  caracteres; nomes longos ou que colidem após a sanitização recebem um sufixo
  de 8 hex do SHA-256 de `servidor\0tool` (todos os envolvidos), e um nome nunca
  muda de dono durante a sessão. O schema de entrada passa com `type: object` e
  `properties` garantidos; schema acima de 16 KiB vira um objeto permissivo com a
  indicação de `describe`; a descrição é cortada em 2048 caracteres. Só servidores
  prontos declaram tools (por isso a primeira requisição espera os `direct`), no
  máximo 96 por requisição (as demais seguem no gateway), e o conjunto é
  recalculado só quando o estado do manager muda: tools de um `tools/list_changed`
  só entram após a próxima descoberta ou reconexão. Mudar o conjunto muda as
  definições de tools (quebra de cache de prompt): é opt-in. A execução usa o
  mesmo caminho do gateway (cancelamento, sem replay, bloqueio de segredos,
  redaction, limites de saída, progresso e a apresentação de resultado abaixo).
  Argumentos precisam ser um objeto (vazio vale `{}`).
- `hidden`: inalcançável por gateway, `codemode`, busca, listagens e tools
  `direct`; uma chamada é recusada antes de qualquer conexão
  ("hidden by configuration"). Um servidor `hidden` cujas tools estão todas
  ocultas some de `{list:true}` e do awareness; com `tool_exposure` revelando
  alguma, ele continua aparecendo.

Direct e gateway são Auto-only, como toda ação MCP.

**Busca.** Okapi BM25 (k1 1,2, b 0,75, stop words, singular ingênuo, separação
camelCase/snake_case, termos Unicode) sobre o nome da tool, descrição, nomes e
descrições das propriedades do schema de entrada, e nome, descrição e
instruções do servidor; tools `hidden` não entram. É a busca do gateway
(`{query}`) e do codemode (`searchTools`).

**Awareness.** Em Auto, com servidores habilitados, o estanque do canal
("Harness channel: …", no fim da mensagem do usuário) ganha um bloco "MCP
servers": uma linha por servidor habilitado, confiável e não oculto, `- <nome>
(<estado>, <n> tools): <primeira linha da descrição ou das instruções>` (a
contagem aparece com o servidor pronto; até 250 caracteres por linha e 4096 no
bloco; "… N more servers" ao exceder), rotulado como texto não confiável do
servidor, sem caracteres de controle e com redaction. O prompt de sistema e a
definição da tool `mcp` não mudam (cache estável).

<a id="mcp-gateway"></a>

### Gateway `mcp`

Meta-tool Auto-only, argumentos validados antes de qualquer envio (campos
desconhecidos são rejeitados; `arguments` é um objeto):

| Chamada | Efeito |
|---|---|
| `{list:true}` | lista servidores (transporte, estado, contagens, descrição e início das instruções) **sem conectar**; até 64 servidores / 16 KiB |
| `{server, list:true, offset?}` | conecta e lista tools (32 por página; só as visíveis), com descrição e instruções completas (até 4 KiB, não confiáveis) no topo |
| `{query, server?, offset?}` | busca BM25, até 32 por página, JSON `{tools:[{name:"mcp.servidor.tool", description}], next_offset, scope, unsearched_servers?}`; sem `server`, só catálogos já conectados (nunca inicia um servidor; os demais vão em `unsearched_servers`), com `server` conecta/atualiza esse; `query` de 1 a 512 bytes e não combina com os outros campos |
| `{server, tool, describe:true}` | schema de entrada e, se houver, `outputSchema` (JSON, até 16 KiB) |
| `{server, tool, arguments?}` | chama a tool (os argumentos não são validados pelo Slim contra o schema; o servidor decide) |
| `{resources:true, server?, cursor?}` | lista recursos |
| `{resource_templates:true, server?, cursor?}` | lista templates de recursos |
| `{server, uri}` | lê um recurso (URI de até 4096 bytes) |

**Recursos** (formato dos tools de recursos do Codex). Com `server` a listagem
é uma página e `cursor` a continua; sem `server`, lista todas as páginas de cada
servidor **já conectado** e com a capability `resources`, junta tudo e traz os
que falharam em `errors`, os cortados por limite em `truncated` e os não
conectados em `notConnected` (a listagem agregada nunca inicia servidor; nomeá-lo
sim). Entradas `ui://` e `profile=mcp-app` (MCP Apps) ficam de fora. A listagem
completa de cada servidor fica em cache até `resources/list_changed` ou troca de
conexão; um `-32601` em `resources/templates/list` significa "sem templates";
um servidor sem a capability responde "does not offer resources". Servidores com
`exposure = "hidden"` não são acessíveis. Limites: 16 páginas, 1000 entradas por
servidor, 500 por página; texto do servidor de uma linha e sem controle. A
listagem que o modelo recebe é sempre JSON válido de até 12 KiB: o que não cabe
sai da lista, é contado em `omitted` e a listagem completa (redigida) vai para
um artefato (`complete`).

<a id="mcp-resultados"></a>

### Resultado de chamadas e leituras

| Conteúdo MCP | O que o modelo vê |
|---|---|
| texto; recurso embutido de texto | o texto |
| imagem (PNG/JPEG/GIF/WebP legível) | bloco de imagem na mensagem da tool + linha `[image <mime>, <tamanho>]` |
| imagem que não vai inline | artefato (id e caminho no texto) |
| áudio | `[audio <mime> omitted]` |
| blob de tipo textual (`text/*`, `application/json`, `+json`, `+xml`) | decodificado como texto |
| qualquer outro blob | artefato, com id e caminho |
| `resource_link` | `[Resource <uri> "<título>" (<mime>, <tamanho>): <descrição>. Read it with mcp {server:"x", uri:"…"}]` |
| tipo de conteúdo desconhecido | `[unsupported MCP content <tipo>]` |

Várias partes de uma leitura vêm rotuladas pela URI. `structuredContent` só vira
texto (JSON indentado) quando a resposta não tem blocos de `content`; sem
`content` nenhum, o envelope inteiro (sem `_meta`) é o texto; a ponte do
codemode recebe o resultado completo, mas `tools.call` devolve
`structuredContent` quando presente e, senão, o envelope completo (`isError`
lança uma exceção, ver [CodeMode](#codemode)). `isError` marca a chamada como falha e, sem texto,
acrescenta `MCP tool <servidor>/<tool> returned an error`. Resultado vazio vira
`(empty result)`.

Imagens só vão inline se tiverem até 192 KiB em base64 e 2000 px por lado (no
máximo 2 por resultado, 320 KiB no total: a contabilidade de contexto conta cada
caractere base64 como texto, então uma imagem grande poderia estourar a
janela); as demais viram artefato (até 8 blobs por resultado). Imagens só chegam
ao modelo em Anthropic e Codex (Responses); Chat Completions e gateways recebem
o texto com "N image(s) not shown". Texto acima de 14 KiB é gravado inteiro
como artefato (já redigido, até 8 MiB) e o modelo vê o começo e o fim com um
marcador e o ponteiro; o corte respeita fronteiras UTF-8. Falhas viram
`mcp error: …`; interrupções dizem se a chamada foi enviada ou se o resultado é
incerto ("do not replay the call automatically").

### CodeMode

Com MCP habilitado, `codemode` também fica disponível em Auto. Executa o corpo
de uma função JavaScript assíncrona em QuickJS embutido no binário, sem Node.js,
acesso direto a arquivos, rede ou carregamento de módulos. O agente descobre
nomes/schemas com `mcp` e usa `await tools.call('mcp.servidor.ferramenta', args)`.
O retorno é `structuredContent`, quando presente, ou o envelope MCP completo;
`isError` lança uma exceção. O `return` da célula é a saída enviada ao modelo;
os retornos intermediários ficam fora desse contexto. Helpers de descoberta
(síncronos, não consomem o limite de chamadas, nunca mostram tools `hidden`):
`searchTools(query, {limit?, server?})` (BM25; padrão 8, máximo 64) devolve
`[{name, server, tool, description}]` com `name` no formato de `tools.call`;
`describeTool('mcp.servidor.ferramenta')` devolve `{inputSchema, outputSchema?, …}`
ou `null`; `listServers()` devolve nome, status, descrição e instruções. As
chamadas ao host são sequenciais: o interpretador é síncrono, então chamadas em
paralelo (por exemplo `Promise.all` de `tools.call`) não rodam em paralelo. Não
há helpers de recursos (`resources`/`uri` são só do gateway).

`store(chave, valorJSON)` e `load(chave)` mantêm valores na sessão;
`store(chave, null)` remove uma chave, e `load` de chave ausente retorna `null`.
Cada célula tem globais novos. Com sessão durável, os valores são registrados
no journal e restaurados ao retomar ou ramificar, sem reexecutar código ou MCP.
Um `store` concluído permanece aplicado mesmo se o restante da célula falhar.

`mcp` e `codemode` são barreiras seriais no bucket mutating. As chamadas internas
MCP executam em ordem e consomem, individualmente, os limites por turno e por
run, além do slot da célula. Reutilizam a proteção de credenciais e o transporte
cancelável. O journal registra cada chamada com identidade ligada à célula,
início, resultado e duração; a atividade aparece como progresso da ferramenta.

Limites da célula: código de 64 KiB, memória de 32 MiB, prazo de 120 s, 1.024
operações com o host, valores persistidos de 64 KiB no total e JSON de até 1 MiB
por requisição/resposta. Exceder um limite produz erro explícito. Resultados
maiores devem ser reduzidos pelo servidor; o código deve retornar um resumo.
Cancelar interrompe o interpretador e a chamada pendente; efeitos externos já
enviados não são desfeitos. Células interrompidas não são repetidas automaticamente.

### Cancelamento e efeitos externos

Antes de a chamada `tools/call` ser admitida para envio, cancelamento ou deadline
expirado impede o envio desse método. O handshake lazy pode já ter enviado
`initialize` ou `notifications/initialized`; se for interrompido, o resultado
registra o estado de cleanup, inclusive `Unconfirmed` para o transporte HTTP.
Depois que `tools/call` passa à fase de envio, bytes podem já ter sido enviados
mesmo que ainda não haja resposta. O resultado é `OutcomeUncertain`, a execução
não declara rollback e a chamada não é repetida automaticamente (nem na retomada
da sessão).

Quando uma interrupção invalida a conexão, somente a geração afetada é fechada e
seu status passa a `Disconnected` antes do retorno. Uma operação posterior pode
abrir uma nova conexão; ela não repete a chamada incerta. `Confirmed` indica que
o cleanup conseguiu confirmar a parada dos recursos locais que possui, como o
processo stdio e suas threads. `Unconfirmed` informa que essa confirmação não
foi possível. No transporte HTTP, cancelar a espera local não confirma a parada
da requisição remota nem desfaz efeitos já executados pelo servidor.

<a id="mcp-seguranca"></a>

### Segurança e limites

Modelo de segurança (o MCP roda sem allow/deny nem aprovação por chamada; as
barreiras são estas):

- **Confiança**: servidores de projeto só iniciam com o workspace confiável;
  `!comando` e leitura de ambiente também só então.
- **Bloqueio de segredos**: uma chamada de tool do modelo (gateway, direct ou
  `codemode`) cujos argumentos contenham um valor sensível registrado é
  rejeitada ("tool call contains registered sensitive material; use a
  configured credential reference"). Valores sensíveis: credenciais de
  `env`/`headers`, `client_secret` e tokens OAuth.
- **Redaction**: os mesmos valores são removidos de eventos, progresso,
  resultados, listagens, artefatos de texto, `mcp.log` e dos valores que o
  codemode grava (`store` e resultados de chamadas).
- **Texto do servidor é não confiável**: nomes, descrições, instruções, logs e
  resultados são limitados em tamanho, redigidos e rotulados quando vão ao
  modelo; instruções do servidor "não podem sobrepor o pedido do usuário".
  Caracteres de controle só são removidos nos campos de uma linha (bloco de
  awareness, listagens de recursos, linhas `resource_link`, progresso, identidade
  do handshake e `mcp.log`). Blocos de texto de resultados de tool, nomes de
  tools e a primeira linha de descrição nas listagens do gateway, e o rabo do
  stderr de servidores stdio passam como recebidos, só limitados e redigidos.
- **Sem redirects** HTTP; URLs são exibidas sem usuário, senha, query e
  fragmento (a URL configurada continua completa no transporte).
- **Sem reexecução** de chamada com resultado incerto (cancelamento, timeout,
  conexão perdida, sessão expirada), inclusive após retomar a sessão.
- **Auto-only**: nenhuma ação MCP roda em Plan ou Read-only.

| Limite | Valor |
|---|---|
| mensagem JSON-RPC (stdio e HTTP) | 16 MiB |
| rabo do stderr (stdio) | 16 KiB |
| tools por servidor / páginas de `tools/list` | 256 / 8 |
| tools por página de listagem / descrição na listagem | 32 / 80 caracteres |
| `describe` | 16 KiB |
| `query` de busca | 1-512 bytes |
| instruções do servidor / identidade do servidor | 4 KiB / 128 caracteres |
| `description` configurada | 2 KiB |
| awareness | 250 caracteres por linha, 4096 bytes |
| tools `direct` por requisição | 96 (descrição 2048 caracteres, schema 16 KiB) |
| progresso | 512 caracteres, 1 evento/100 ms, fila de 16 |
| `mcp.log` | entrada 8 KiB, rotação em 5 MB |
| texto inline por resultado | 14 KiB (artefato de até 8 MiB com o texto inteiro) |
| imagens inline | 2 por resultado, 192 KiB cada, 320 KiB total, 2000 px |
| blobs em artefato por resultado | 8 |
| listagem de recursos para o modelo | 12 KiB; 16 páginas, 1000 por servidor |
| conexão HTTP | 2 retentativas; DELETE 1 s; `cancelled` 500 ms |
| OAuth | requisição 15 s, resposta 256 KiB, login 5 min, lock obsoleto 20 s |
| codemode | ver [CodeMode](#codemode) |

<a id="mcp-nao-suportado"></a>

### Não suportado

- **Prompts** (`prompts/list`, `prompts/get`), **sampling**, **elicitation**,
  **completion**, **tasks**, `logging/setLevel` e `resources/subscribe`: o cliente
  não os oferece; pedidos do servidor correspondentes recebem `-32601`.
- **MCP Apps** (`ui://`, `profile=mcp-app`): filtrados das listagens de recursos.
- **WebSocket** e o transporte **SSE legado** (HTTP+SSE de `2024-11-05`;
  `import` pula esses servidores). A *versão* `2024-11-05` do protocolo é aceita
  sobre stdio e Streamable HTTP.
- Revisões de protocolo fora das quatro aceitas (por exemplo `2026-07-28`).
- **Approval prompts** e allow/deny por tool: tools MCP rodam em Auto sem
  confirmação; use `exposure = "hidden"` para tirá-las do alcance do modelo.
- **`env_clear`** para stdio: o servidor herda o ambiente do processo.
- **Áudio** ao modelo (vira `[audio … omitted]`) e imagens em providers cujo wire
  não as aceita em resultados de tool.
- **Helpers de recursos no codemode** e **chamadas paralelas** de `tools.call`.

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
| Skills, MCP, subagentes | Tool `skill` lazy (`list` / `name`); roots `.slim/skills`, `.claude/skills`, `.agents/skills` e `.codex/skills` no projeto, `.slim/skills`, `.agents/skills` e `.codex/skills` no perfil (nesta ordem de prioridade; o projeto vence o perfil). Scripts chamados pelo modelo exigem trust explícito e são recusados sem grant; leitura de `SKILL.md` continua disponível. MCP stdio/Streamable HTTP com conexão em segundo plano, meta-tool `mcp`, tools `direct`, `codemode`, CLI `slim mcp` e overlay `/mcp` (ver [seção MCP](#mcp)). Sem child. |
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

O runtime aplica budgets separados read-only vs mutating por run (`max_read_tool_calls` default 96, `max_mutating_tool_calls` default 32; env `SLIM_MAX_READ_TOOL_CALLS` / `SLIM_MAX_MUTATING_TOOL_CALLS` e chaves `slim.toml`); esgotamento → `tool_limit` (exit 22). O teto de turns por run é 128 (`SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024); esgotamento → `turn_limit` (exit 12) com mensagem `Turn limit reached (N/N)`. Quando o teto por turno corta calls de um batch, a mensagem seguinte ao modelo lista cada call não executada (nome e argumentos abreviados) para que só elas sejam repetidas. A tool `search` respeita `.gitignore`, ignora árvores de build, cap default 200 hits e paginação `offset`/`max_hits`. Emite `ToolStarted`, mantém pares assistant/tool através da compactação (veja "Compactação de contexto" abaixo).

Cada valor de API key fornecido é redigido exatamente antes
de tool output, follow-up ao provider, renderização e sessão; isso não promete
encontrar qualquer segredo arbitrário que não tenha sido registrado.

### Compactação de contexto

O Slim tem um único modo de compactação: o método padrão do Pi
(`coding-agent`, licença MIT), portado em Rust. Não existem estratégia
alternativa, compactação em background, previsão de custo nem resumo local sem
modelo. O resumo vem sempre do modelo da própria sessão, em primeiro plano, com
as tools desligadas e o esforço de raciocínio da sessão.

**Configuração.** `[compaction]` em `slim.toml` aceita:

| Chave | Padrão | Efeito |
|---|---|---|
| `enabled` | `true` | liga a compactação automática (limiar e recuperação de overflow); `/compact` funciona mesmo com `false`, como no Pi |
| `reserve_tokens` | `16384` | margem abaixo da janela: define o gatilho e o orçamento de saída do resumo; deve ser positivo |
| `keep_recent_tokens` | `20000` | tokens das mensagens mais recentes mantidas literalmente; valor fixo, sem teto relativo à janela; deve ser positivo |
| `summary_max_bytes` | `65536` | limite do resumo persistido, listas de arquivos incluídas (1 a 65536) |
| `manual_instructions_max_bytes` | `4096` | limite das instruções de `/compact` (1 a 4096) |

As chaves antigas `background` e `strategy` não existem mais: continuam sendo
aceitas no arquivo, mas ignoradas.

**Gatilho.** Antes de cada requisição o runtime compacta quando
`context_tokens > context_window - reserve_tokens` (comparação estrita). O
contexto é o `usage` da última resposta do provider mais uma estimativa
`ceil(caracteres / 4)` das mensagens posteriores (texto, raciocínio legível,
nome e argumentos de tool calls; imagem, áudio e arquivo contam 4800
caracteres). O `usage` anterior à última compactação não vale. Sem `usage`
(primeira requisição de uma execução, ou logo após compactar) o contexto é a
estimativa pura, incluindo o system prompt e os schemas das tools.

A âncora de `usage` é lembrada entre execuções pelo handle de compactação da
sessão (a TUI mantém um por sessão): a primeira requisição de um novo prompt já
parte do `usage` da última resposta, desde que essa resposta continue na mesma
posição do histórico e o modelo seja o mesmo; compactar ou elidir saídas antigas
(inclusive a elisão de uma leitura substituída por uma escrita no meio da
execução) a descarta. O diário durável não guarda `usage` por chamada, então ao retomar uma
sessão em outro processo a primeira requisição usa a estimativa pura. Uma
resposta aceita cujo `usage` excede a janela (o overflow silencioso do Pi) deixa
a âncora acima do gatilho e a próxima requisição compacta.

O gatilho também dispara quando a requisição, estimada pelo estimador adaptativo
mais a reserva de saída (o limite de saída configurado, que a recuperação de
truncamento pode elevar), não cabe na janela: é o critério do portão de contexto
que vem a seguir, então uma requisição que o portão recusaria é compactada
antes. Para uma janela pequena demais para
descer abaixo do gatilho, há uma única proteção contra laço: a compactação
automática só roda de novo depois que ao menos uma resposta foi anexada à
conversa. Compactação manual e por overflow ignoram essa espera.

Como no Pi, uma compactação automática que falha (erro do provedor, resumo
truncado, vazio ou grande demais) não derruba a execução: o runtime avisa
`Auto-compaction failed: ...` (a TUI mostra uma notificação e a CLI sem TUI
imprime `warning: Auto-compaction failed: ...` em stderr ao fim da execução),
envia a requisição como está e só tenta de novo depois da próxima
resposta. Isso vale quando só a linha do Pi disparou o gatilho; se a requisição não cabe no portão de contexto, se a compactação foi
pedida (`/compact`) ou veio de um overflow, a falha encerra a execução, e o
portão de contexto ainda recusa uma requisição que realmente não cabe.

**Ponto de corte.** Só mensagens de usuário e de assistente são pontos de
corte; um resultado de tool nunca é. O Slim percorre o histórico do fim para o
início somando tokens estimados até alcançar `keep_recent_tokens` e corta no
primeiro ponto válido nessa posição ou depois. Garantias próprias do Slim:
chamada e resultado de tool nunca ficam separados, e mensagens
system/developer nunca entram no resumo. O trecho resumido começa depois do
resumo anterior. Se o corte cai no meio de um turno (a primeira mensagem
mantida não é do usuário), o prefixo desse turno vira um **segundo resumo**
(prompt de prefixo de turno, orçamento de 50% da reserva) e os dois textos são
unidos com `---` e `**Turn Context (split turn):**`; sem histórico anterior o
primeiro texto é `No prior history.`.

**Prompts.** Os textos são os do Pi, byte a byte: system prompt de resumo,
prompt de resumo com as seções `Goal`, `Constraints & Preferences`, `Progress`
(`Done`/`In Progress`/`Blocked`), `Key Decisions`, `Next Steps` e
`Critical Context`, e o prompt de atualização quando há resumo anterior (enviado
em `<previous-summary>`, com a conversa em `<conversation>`). Instruções de
`/compact` entram como `Additional focus:` na chamada do histórico. A conversa é
serializada com os rótulos do Pi (`[User]`, `[Assistant]`, `[Tool result]`...) e
resultados de tool são cortados em 2000 caracteres. O limite de saída é
`min(0,8 × reserve_tokens, limite de saída da requisição)` para o histórico e
`0,5 × reserve_tokens` para o prefixo; esse teto só baixa o limite de saída já
configurado (o `max_output_tokens` por turno, 4096 por padrão em modelos sem
catálogo). Um resumo que para por limite de saída é reenviado uma vez com o
limite dobrado, até o teto do orçamento, metade da janela e o máximo do modelo
quando o catálogo o conhece; se ainda assim truncar, a compactação falha com a
sugestão de subir `compaction.reserve_tokens` (o teto é 80% da reserva para o
histórico e 50% para o prefixo de um turno dividido); subir `max_output_tokens`
só ajuda enquanto ele estiver abaixo desse teto. Se a conversa serializada não cabe na
janela, as pontas são aparadas; só falha se as partes fixas do prompt não
couberem.

**Arquivos.** As chamadas `read` contam como lidas e `write`/`patch` como
modificadas (modificado vence lido). Após o texto do modelo o Slim acrescenta os
blocos `<read-files>` e `<modified-files>`, cumulativos desde o resumo anterior,
no máximo 256 caminhos por classe, 4 KiB por caminho e 64 KiB no total; os
excedentes saem do mais antigo para o mais novo.

**Resultado.** A resposta é rejeitada se parar por motivo não terminal, por
limite de saída, por filtro, com tool call ou vazia (cada chamada tem retry
limitado de erros recuperáveis e é uma requisição `Compaction` no ledger de
usage, a do prefixo incluída). O texto é redigido e precisa caber em
`summary_max_bytes`; se só o texto não couber a compactação falha, e nunca se
grava um registro que a sessão rejeitaria. O histórico passa a ser as mensagens
system/developer, uma mensagem de usuário com o prefixo `The conversation
history before this point was compacted into the following summary:` e o resumo
entre `<summary>` e `</summary>`, e as mensagens mantidas. O checkpoint durável
guarda o resumo, as listas de arquivos, o motivo (`threshold`, `manual`,
`overflow` ou `branch`; os antigos `soft_threshold` e `hard_threshold` leem como
`threshold`), tokens antes/depois, fingerprint do prefixo e a cadeia de
checkpoints. Checkpoints antigos carregam normalmente e recebem o mesmo
envoltório; o resumo anterior e as listas de arquivos são derivados do próprio
histórico, então sobrevivem a retomadas e a novas execuções. O fingerprint do
prefixo cobre só o que o diário registra (papel, nome, tool calls, blocos e
conteúdo; sem raciocínio opaco, snapshot do workspace, instruções de skill
anexadas ao prompt nem ponteiros de elisão), para que o histórico vivo e a
reconstrução concordem. Os resultados de um batch paralelo são gravados na
ordem de conclusão e vivem na ordem das calls, então o commit compara o
fingerprint canônico (resultados consecutivos ordenados pelo id da call) e o
checkpoint guarda o fingerprint na ordem do diário. Checkpoints do escritor
anterior, cujo fingerprint cobria os bytes crus do diário (sem redação de
credenciais), também verificam: a reconstrução aceita o fingerprint atual ou
o cru. Um ponteiro `[duplicate <tool> result omitted; ...]` mantido cujo
resultado original foi resumido vira uma nota de que a saída idêntica foi
compactada (o diário guarda o ponteiro original), tanto na compactação quanto
na reconstrução. Cadeias de checkpoints do
escritor anterior (dois ou mais) também carregam, e o branch compactado encadeia
no último checkpoint realmente aplicado. Checkpoints aplicados são gravados
também quando a execução falha ou é cancelada; um que não ancora mais no diário
é descartado e avisado (notificação na TUI, aviso em stderr no headless), e a
sessão continua válida. O branch compactado usa o mesmo resumidor e os mesmos
prompts.

**Overflow.** Um erro de contexto excedido do provider (padrões do Pi mais os
códigos de erro do provider; o Pi não olha o status HTTP, o Slim ignora os que
não são 4xx e 401, 403, 408 e 429, para não confundir falha de servidor,
autenticação ou limite de taxa com overflow; o caso bodyless do Cerebras não é
portado, pois o Slim não identifica o provider no erro) compacta e reenvia uma
vez por episódio; com `[compaction] enabled = false` o overflow não compacta e o
erro sai como veio. Se o overflow persistir, o erro recebe o texto do Pi sobre a
tentativa única. O corte por limite de saída continua tendo recuperação
própria (veja acima).

**`/compact [instruções]`.** Com a sessão ociosa, a compactação roda
imediatamente como uma execução própria (cancelável, durável, com usage), sem
turno de modelo; a TUI mostra o bloco "compactação concluída" com os tokens
antes → depois. Sem o que compactar, avisa `Nothing to compact (session too
small)` ou `Already compacted` (nada foi anexado desde a última compactação; o
Slim põe o resumo no começo do histórico, então isso é detectado pelo handle,
não por o resumo ser a última mensagem). Durante uma execução ativa o pedido fica
enfileirado e roda no próximo ponto seguro (divergência documentada em relação
ao Pi). `enabled = false` não impede o comando: só desliga os gatilhos
automáticos. Uma execução de `/compact` cancelada ou que falha não deixa o
pedido enfileirado.

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
