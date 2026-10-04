# Proposta: LSP nativo e automático para JavaScript e TypeScript

Status: implementação nativa com gates reais Rust e JS/TS validados em
**01/10/2026**. Publicação vigente, versões testadas, comandos, resultados e
limites ficam em [release/README.md](../release/README.md).
As decisões abaixo registram a revisão anterior à implementação; Python e os
protocolos explicitamente condicionados continuam fora desta primeira entrega.
Referência OMP: commit `9d8d72524fd733e38ff1af1b8c387ed01e0b1a74`.

## 1. Resultado esperado

JavaScript e TypeScript devem funcionar na integração LSP nativa do Slim sem
ativação, plugin, comando de habilitação, flag ou configuração obrigatória.
A descoberta e o início do servidor são automáticos quando uma consulta
semântica precisar dele. Rust continua funcionando pelo mesmo manager.

"Nativo" significa que descoberta, transporte, pool, sincronização, ferramenta
e integração com o agente pertencem ao Rust do Slim. O TypeScript Language
Server e o motor TypeScript continuam processos/dependências externos: esta
proposta não implementa um analisador TypeScript dentro do Slim nem incorpora
um harness Node intermediário.

A configuração existente serve apenas a overrides avançados ou desativação
explícita. Não faz parte do caminho normal do usuário. Ausência de dependências
gera indisponibilidade explicada; não provoca instalação nem download.

A primeira entrega cobre TypeScript Language Server para `.js`, `.jsx`,
`.mjs`, `.cjs`, `.ts`, `.tsx`, `.mts` e `.cts`. Declarações
`.d.ts`, `.d.mts` e `.d.cts` seguem a extensão final. Python fica para
depois, usando os mesmos mecanismos.

Cobertura: definição, referências, hover, símbolos, diagnósticos explícitos,
sincronização de write/patch, mudanças externas e relatório pós-edição em lote
misto. Não promete validação integral de tipos, build ou execução de testes.

Ficam fora: instalar dependências, alterar configuração global, ESLint/Biome,
Deno, frameworks com servidores próprios, TypeScript nativo sem tsserver
compatível, rename, aplicação de code actions, formatação, checkers externos,
hot reload da configuração do Slim, entrega tardia e mux entre aplicações.

## 2. O que a revisão do código mudou na proposta

| Constatação | Decisão corrigida |
|---|---|
| O catálogo chama `supports_workspace(cwd)` antes de anunciar `code_intel` | A capacidade nativa deve continuar visível sem depender de marcadores ou de um servidor já iniciado |
| `ServerInstanceConfig` já separa initialization options e settings, mas o pool duplica um único payload nos dois | Corrigir a passagem no pool; não criar outra camada de configuração na instância |
| `PoolKey` contém raiz, ID e hash apenas do payload atual | A identidade efetiva também inclui comando, args e os dois objetos de configuração |
| O transporte recebe um callback síncrono de pedidos server→client | Resolver folders/settings nesse callback; não convertê-lo em handler async para adicionar JS/TS |
| As consultas e `CodeIntelMeta` identificam um servidor | Manter uma consulta por servidor; filtro opcional nas consultas sem arquivo, evitando agregação/paginação nova |
| O process factory já usa comando e args separados | Resolver o entrypoint Node antes de spawn; manter o factory existente |
| O store de diagnósticos, baseline e progresso estão na instância | Manter isolamento por processo; não compartilhar estado semântico entre linguagens |
| A instância usa a raiz como fronteira de paths e URIs | JS/TS usa o workspace autorizado como raiz; não criar uma instância por tsconfig aninhado |
| O snapshot atual não interpreta configurações TypeScript | Usar observação conservadora de inputs, sem implementar um parser JSONC/grafo de extends |
| A nota pós-edição só é injetada quando haverá próxima chamada ao provider | Preservar esse ponto; não prometer verificação automática depois de toda resposta final |

Referências: [catálogo do runtime](../crates/slim-core/src/runtime/mod.rs),
[loop](../crates/slim-core/src/runtime/agent_loop.rs),
[manager](../crates/slim-lsp/src/manager.rs),
[pool](../crates/slim-lsp/src/pool.rs),
[instância](../crates/slim-lsp/src/instance.rs),
[transporte](../crates/slim-lsp/src/transport.rs),
[ferramenta](../crates/slim-core/src/tools/code_intel.rs) e
[fronteira LSP](../crates/slim-lsp/src/path_policy.rs).

A tabela registra o diagnóstico do código anterior à implementação. A primeira
entrega aplicou essas decisões reutilizando os componentes indicados; não se
trata de uma lista de defeitos ainda presentes no checkout.

## 3. Arquitetura e estruturas mínimas

Fluxo: ferramenta preparada → seleção do perfil → resolução efetiva → lease
do pool → refresh → sincronização → consulta → resultado do servidor correto.

Preservar `CodeIntelligence`, as seis ações e o transporte stdio. A seleção
fica em `slim-lsp`; o core conhece apenas os pedidos e os resultados. TUI e
headless continuam passando o mesmo manager conforme seu ciclo atual.

Usar dois perfis internos: `rust-analyzer` e
`typescript-language-server`. Reutilizar `ServerSpec` para ID, comando,
args, marcadores e IDs de linguagem. Associar ao perfil a política de snapshot
e os defaults de configuração. Não criar trait por linguagem, plugin registry,
catálogo externo nem trait adicional para descoberta.

Uma resolução deve fornecer, juntos, raiz canônica, spec efetivo,
initialization options e settings. Um pequeno struct interno é suficiente.
A mesma resolução alimenta aquisição normal, aquisição warm, status e
sincronização; esses caminhos não podem montar identidades diferentes.

Generalizar `LspManagerConfig` de `server_path/server_config` singulares
para opções por perfil. Manter idle, timeout, max_servers e max_open_documents
no nível comum. Adaptar os fixtures existentes nessa mudança; não sustentar
dois mecanismos concorrentes de configuração para evitar alterações mecânicas.

Continuar com `PoolKey { root, server_id, config_hash }`. O hash pode
representar um payload normalizado com comando absoluto, args efetivos,
initialization options e settings completos. Não precisa de novo algoritmo de
hash ou cache separado. Centralizar a construção da chave e reutilizá-la em
`acquire`/`acquire_warm`, inclusive status e pós-edição.

Comandos ou configurações diferentes não compartilham um processo; resoluções
equivalentes compartilham a inicialização em andamento. Preservar leases,
backoff, encerramento e limite global do pool.

O limite de processos é global ao manager; o limite de documentos abertos e
os budgets de transporte/store continuam por instância. Duas linguagens podem
portanto aumentar o consumo total dentro desses limites. Não descrevê-los como
um orçamento global de memória nem duplicar valores para manter um único cache.

## 4. Raiz do projeto e roteamento

JS/TS terá uma instância por workspace canônico e configuração efetiva.
O TypeScript Language Server recebe essa raiz como cwd/rootUri/workspace folder.
O motor TypeScript decide o tsconfig/jsconfig aplicável a cada arquivo, incluindo
configurações aninhadas e project references.

Não escolher um novo processo a cada tsconfig próximo. Isso multiplicaria
processos e, com a fronteira atual da instância, poderia rejeitar definições em
pacotes irmãos que pertencem ao workspace autorizado.

Rust mantém sua descoberta Cargo atual. Não mudar a busca por ancestrais de
Rust nem relaxar `path_policy` para viabilizar JS/TS. Arquivos e resultados
JS/TS fora do workspace continuam fora da cobertura, mesmo se um tsconfig ou
um import os referenciar.

Consultas com arquivo selecionam o perfil pela extensão; validar o path
preparado antes de resolver ou iniciar qualquer servidor. Arquivo não servido
retorna motivo explícito sem tentar todos os servidores. Usar os language IDs:
`javascript`, `javascriptreact`, `typescript` e `typescriptreact`.

`package.json`, `tsconfig.json` e `jsconfig.json` identificam projeto
configurado, mas não são condições para uma consulta de arquivo JS/TS.
Sem marcador, o servidor pode administrar um projeto inferido na mesma raiz.
Não criar config, forçar checkJs nem alterar opções do projeto.

JS sem checkJs pode ter diagnósticos de tipos limitados por decisão do projeto.
Sintaxe, navegação e tipos disponíveis devem ser testados sem transformar essa
limitação em erro do Slim ou afirmar que todos os checks foram executados.

## 5. Descoberta automática e lançamento

Resolver o TypeScript Language Server nesta ordem:

1. Override de path, se fornecido.
2. Instalação no `node_modules` da raiz do workspace.
3. Instalação encontrada por PATH.

A primeira etapa não recorre silenciosamente às demais quando um path explícito
é inválido. Essa é uma mudança intencional em relação ao helper atual, que cai
para PATH; preservar a semântica de paths explícitos válidos e cobrir a mudança
por teste. Não exigir que PATH exista para aceitar um path válido.

Não procurar pacotes npm recursivamente em todos os subdiretórios. A resolução
automática inicial usa a instalação da raiz; uma instalação isolada em pacote
aninhado é descoberta ao abrir esse pacote como workspace, ou por override
opcional. Não interpretar workspaces npm/pnpm/Yarn para montar um grafo paralelo.
Instalações PnP sem launcher/entrypoint comum ficam fora da autodetecção inicial.

Separar executável do servidor e runtime Node. No Windows, um `.cmd`
encontrado não comprova que o factory conseguirá iniciá-lo. Para o pacote
padrão, resolver o `bin` declarado no package.json instalado, validar o nome
do pacote, o entrypoint existente e sua contenção no pacote canônico; executar
`node.exe <entrypoint> <args-do-servidor>` com argumentos separados.

Aplicar o mesmo caminho quando o override apontar ao entrypoint JS conhecido.
Para launcher npm padrão no PATH, localizar o pacote instalado correspondente,
sem executar nem interpretar o texto do shim. Se não houver mapeamento
verificável, informar a limitação. Não introduzir cmd/PowerShell como shell
genérico nem aceitar launchers arbitrários como se fossem executáveis nativos.

`ServerSpec.command` pode ser o Node resolvido e `args` conter o
entrypoint seguido de `--stdio`. Assim, `StdioProcessFactory` e
`ProcessFactory` continuam os mesmos. O prefixo entrypoint é interno;
override de args substitui somente os argumentos do servidor.

Não assumir que encontrar Node ou o servidor garante um motor TypeScript
compatível. Inicialização e testes reais precisam confirmar tsserver funcional.
Não selecionar `tsc --lsp` nem trocar de servidor silenciosamente em
instalações sem `tsserver.js`.

A integração aprovada usa o TypeScript clássico com `lib/tsserver.js`.
TypeScript nativo sem esse motor fica fora do contrato; a disponibilidade de
um pacote TypeScript isoladamente não comprova compatibilidade.

A resolução cacheada deve reavaliar a precedência local/PATH e a existência do
Node/entrypoint. Uma instalação local surgida após a inicialização do Slim não
pode ficar escondida por um cache positivo antigo. Não cachear falha de
descoberta indefinidamente. Atualização de dependências durante a sessão continua
com os limites de frescor da seção 8.

## 6. Configuração opcional e defaults

Nenhum TOML é necessário para usar JS/TS. Os perfis vêm habilitados por padrão,
com resolução automática e args padrão. Preservar `lsp.enabled = false`
e desativação explícita por servidor; desligar Rust não desliga TypeScript.

Manter o formato layered em `FileLspServerConfig`, `LspServerConfig`
e `merge_layer`. Acrescentar apenas `args`,
`initialization_options` e `settings` opcionais. Ausência em uma camada
mantém a anterior; presença substitui o campo inteiro. Não fazer deep merge de
objetos nem transformar campo ausente em objeto vazio antes da fusão.

Validar os tipos dos novos campos conhecidos. Chaves desconhecidas continuam
ignoradas conforme `FileConfig`; não introduzir rejeição global de chaves
para implementar LSP. IDs não pertencentes aos perfis iniciais não cadastram
servidores novos; devem ser distinguíveis no diagnóstico de configuração/status.

`settings` será o objeto completo por seção, como
`{ "typescript": { ... }, "javascript": { ... } }`, e não um único valor
embrulhado sob `typescript-language-server`. Em Rust, adaptar os defaults
para `{ "rust-analyzer": { "checkOnSave": false } }`; as initialization
options conservam seu objeto próprio. A instância já tem campos separados;
o pool precisa deixar de duplicar o mesmo payload.

Na entrega atual, os defaults JS/TS são
`{"hostInfo":"slim","tsserver":{"useSyntaxServer":"never"}}` e settings `{}`.
O gate real confirmou o uso do tsserver semântico para a primeira consulta.
Overrides de initialization options substituem esse objeto inteiro.

Exemplo exclusivamente de override avançado, suportado pela primeira entrega:

```toml
[lsp.servers.typescript-language-server]
path = "C:/ferramentas/typescript-language-server/lib/cli.mjs"

[lsp.servers.typescript-language-server.initialization_options]
hostInfo = "slim"

[lsp.servers.typescript-language-server.initialization_options.tsserver]
useSyntaxServer = "never"

[lsp.servers.typescript-language-server.settings.typescript]
# Opções suportadas pelo servidor podem ser declaradas aqui.
```

Não adicionar `/lsp on`, wizard, novos flags CLI nem dependência de plugin.
Não implementar hot reload dos arquivos do Slim; os ciclos existentes da TUI
e do headless continuam responsáveis por carregar a configuração.

## 7. Compatibilidade do cliente: necessário e condicionado

Primeira entrega:

- Responder `workspace/workspaceFolders` com a raiz da instância, coerente
  com o que já foi enviado no initialize.
- Resolver `workspace/configuration` no objeto completo de settings,
  alinhando itens, suportando seções/subseções e retornando null quando ausentes.
  Não ignorar diferenças entre `typescript` e `javascript`.
- Enviar `workspace/didChangeConfiguration { settings: ... }` depois de
  initialized, antes de sincronização/consultas, quando houver settings.
- Preservar negociação estática de sync/save e posições, tratamento de EOF,
  cancelamento, caps de frames/filas e descarte de respostas tardias.
- Identificar o servidor efetivo nos metadados e erros, inclusive caminhos
  degraded/unavailable. Não deixar strings hardcoded de rust-analyzer.

O handler de pedidos é síncrono e não deve fazer IO nem adquirir o mutex
assíncrono da instância. Folders/settings podem ser dados imutáveis capturados
na criação, reutilizando a arquitetura do transporte.

Registro dinâmico e pull deixam de ser gates incondicionais de JS/TS. Primeiro
validar o servidor alvo pelo fluxo push/static. Só implementar a capacidade
extra se ele realmente precisar dela para cumprir os critérios. Isso evita
desenvolver uma infraestrutura LSP 3.17 inteira antes da primeira integração real.

Se registro dinâmico for necessário, delimitar métodos, quantidade,
documentSelector e lifecycle; anunciar apenas o suporte implementado.
Não confirmar um pedido de registro que o Slim não conseguirá cumprir.
Manter watcher dinâmico desativado: eventos conhecidos e snapshot não são um
watcher geral. Não aceitar registros genericamente para destravar o handshake.

Se pull for necessário, reutilizar os tipos já disponíveis em lsp-types 0.97:
full/unchanged, result ID, identifier do provider e relatedDocuments. Respeitar
fronteira e limites também nos documentos relacionados. Não converter pull em
publishDiagnostics com uma versão inventada.

`workspace/applyEdit` continua recusado. Requisições arbitrárias,
executeCommand genérico e mutações LSP ficam fora. A implementação futura de
qualquer capacidade deve preservar os modos e a admissão do core; esta revisão
documental não altera funções de segurança/delimitação.

## 8. Sincronização, snapshot e custo

Reutilizar DocumentStore, digest, versões, LRU e didOpen/didChange/didSave/didClose.
O primeiro open envia conteúdo completo; mudanças posteriores seguem a
negociação atual e usam o patch existente quando seu digest for compatível.
Não reenviar full text por hábito nem chamar a sincronização a partir de um
segundo sistema de escrita.

Generalizar `workspace::snapshot/project_input` conforme o perfil. Rust
conserva sua política. Em JS/TS, observar fontes atendidas, arquivos .json/.jsonc
e lockfiles dentro da raiz: package-lock.json, npm-shrinkwrap.json, yarn.lock,
pnpm-lock.yaml, bun.lock e bun.lockb. Lockfiles binários exigem stamp, não parsing.

Tratar .json/.jsonc conservadoramente como entradas estruturais na primeira
entrega. Isso cobre package.json, configurações com nomes arbitrários usadas
por extends e resolveJsonModule sem escrever um parser de comentários,
trailing commas ou project references. Mudança nesses inputs retira a geração.
É uma escolha simples que pode reiniciar o servidor em edições de JSON de dados;
medir o custo antes de estreitar a política. Não prometer seguir um grafo que
o Slim ainda não interpreta.

Excluir .git, .slim e node_modules. Em workspace misto, excluir também target
identificado como saída Cargo, para não atravessar o build Rust e esgotar o cap
da varredura JS/TS. Não excluir genericamente dist/lib/declarações: um projeto
pode importar seus próprios .d.ts nesses diretórios. A política precisa ser por
perfil e suas exclusões devem aparecer como limites de observação; não copiar
nomes de diretórios de build indiscriminadamente.

Manter a varredura bounded e o erro explícito atual quando o limite é excedido.
Reutilizar spawn_blocking; sem watcher, polling ocioso, índice persistente ou
cache global novo. FileStamp não detecta alterações que preservem todos os
metadados observados.

Antes de consultar, reconciliar criação/remoção/edição externa e documentos
abertos. Eventos conhecidos devem preceder a consulta. Reutilizar a invalidação
de dependentes preservando a publicação fresca de um arquivo já sincronizado.

A recepção de watchedFiles não comprova que TypeScript recarregou seus projetos.
Testar imports e módulos criados/removidos com servidor real. Se o caminho
existente não atualizar corretamente, retirar a geração nessas mudanças
estruturais e reabrir na próxima consulta; não adicionar um comando privado
do servidor sem comprovar necessidade/semântica.

Snapshot é observação no início da operação, não transação atômica do filesystem.
Conferir versão e stamp do documento ao produzir o resultado e preservar
staleness para mudanças conhecidas. Não alegar que dependências externas ou
alterações simultâneas de arquivos fechados foram todas observadas durante o RPC.

Conteúdo em node_modules, libs do runtime/servidor, configs fora da raiz e
dependências atualizadas sem input observado não ganham garantia de frescor.
Nesses casos, reiniciar a aplicação é o procedimento inicial; não afirmar
reload automático de dependências. Uma atualização detectada por lockfile
retira a geração, mas sua aplicação efetiva deve ser comprovada no teste real.

## 9. Ferramenta: manter consultas de um servidor

Manter `action=symbol` (singular), definition, references, hover,
diagnostics e status. Atualizar a descrição atual "Semantic Rust" para a
cobertura real. Nenhuma ação nova é necessária para JS/TS.

Consultas com path são automáticas pela extensão. Consultas sem path precisam
de uma seleção que não dependa de ordem de cadastro nem de qual processo
foi iniciado primeiro. Acrescentar `server` opcional somente às queries
de símbolos/diagnósticos:

- Com path, servidor omitido é inferido; filtro explícito incompatível é erro.
- Sem path e com server, consultar esse perfil na raiz da operação.
- Sem path nem server, usar o único perfil aplicável ao workspace.
- Se houver mais de um perfil aplicável, retornar seleção ambígua com seus IDs;
  o agente repete a consulta com server. O usuário não habilita nada.

Aplicabilidade para consulta sem path considera perfis habilitados e marcadores
de projeto: preservar a descoberta Cargo; para JS/TS considerar os marcadores
na raiz do workspace. Sem nenhum marcador, o perfil TypeScript habilitado pode
servir como projeto inferido. Não escolher pelo estado warm, disponibilidade de
binário ou ordem de inicialização: isso mudaria o servidor de uma consulta
conforme a história da sessão. Não varrer subprojetos para preencher essa lista.

Exemplo do contrato implementado:

```json
{"action":"definition","path":"web/src/main.ts","line":8,"column":3}
{"action":"symbol","query":"createClient","server":"typescript-language-server"}
{"action":"diagnostics","server":"rust-analyzer"}
```

Essa decisão substitui a agregação multiprocesso da versão anterior da proposta.
`CodeIntelOutcome`/`CodeIntelMeta` continuam identificando um servidor
por consulta; paginação existente não precisa combinar gerações de vários
processos. O filtro não é ativação nem configuração de usuário.

Atualizar juntos schema, parser público, parser prepared, `native_argument_keys`,
allowlists de `canonicalize_code_intel_arguments`, queries tipadas e testes de
admissão. A preparação rejeita campos desconhecidos antes do parser; depois,
a canonicalização pode remover o filtro se não o reconhecer. Testar o caminho
completo até a query tipada. Respeitar additionalProperties e combinações por ação.

Tokens de referências/símbolos continuam vinculados à revisão e geração
selecionadas. Estender a identidade ao perfil/query efetivo onde necessário;
uma continuação de Rust não pode servir à mesma query TypeScript.

Na entrega atual, diagnóstico explícito de arquivo usa uma espera curta pela
publicação, limitada pelo deadline da operação. Resposta sem publicação traz
received=false/completeness=unknown. Publicação sem versão pode ser exibida,
mas conserva diagnostic_version=null/completeness=unknown; document_version
identifica o documento sincronizado, sem certificar a versão da publicação.

Diagnósticos sem path continuam sendo o conjunto publicado/conhecido do
servidor, como no fluxo atual; não iniciar um diagnóstico de cada arquivo
da árvore nem apresentar esse conjunto como verificação integral do workspace.

## 10. Disponibilidade automática e apresentação

Construir o manager com todos os perfis padrão habilitados, independentemente
de os executáveis já existirem. `supports_workspace` não deve esconder a
ferramenta por ausência de Cargo.toml/tsconfig ou binário: com manager ativo e
perfil habilitado, anunciar a capacidade respeitando o modo operacional.
A resolução real ocorre na consulta; status pode explicar o que falta.

Essa é mudança deliberada no comportamento atual de anúncio. Não requer
alterar o mecanismo do catálogo; pode ser implementada no backend e validada
com os testes de compartilhamento de schemas já existentes. Não fazer
varredura recursiva nem spawn em `supports_workspace`.

Status usa a lista `servers` já existente: perfil/ID, raiz, instalação
resolvida e estado factual, incluindo ausência do Node/entrypoint, desativação
e falta de projeto. Status não inicia processos; consultar um processo warm
continua usando a mesma chave efetiva.

Distinguir disponível/configurado de iniciado. O enum atual não possui
"available"; não usar Starting como prova de handshake em andamento apenas
porque um binário foi encontrado. Manter o estado por servidor no payload
e um resumo agregado conservador, sem criar um enum inteiro para essa tarefa.

A apresentação de status hoje omite o ID da linha; acrescentá-lo. Mostrar o
servidor e os paths de forma bounded/redigida, sem despejar comandos internos.
Reutilizar registros, budget e rendering do core; não mudar layout/motion da TUI.

Progresso atual reconhece tokens Rust específicos e números. Não interpretar
qualquer progresso TypeScript como certificação de indexação completa. Ready
com completeness=unknown é válido; versão de diagnóstico fresco e completude
de indexação do workspace são evidências distintas.

## 11. Pós-edição e confiabilidade

Manter warm-only para aquisição automática pós-edição. Não iniciar processos
para arquivos que o agente apenas escreveu. Consulta semântica explícita
inicia automaticamente quando necessário; não depende de ativação pelo usuário.
Sem servidor warm, a nota relata não verificado. Essa diferença deve aparecer
na documentação e nos testes, sem prometer verificação automática universal.

Deduplicar paths canônicos e classificar antes de agrupar. Guardar paths de
apresentação relativos ao workspace da operação, evitando colisão entre
`src/main.rs` e outro `src/main.ts` sob raízes diferentes.

Usar um deadline absoluto para todo o lote: os 1,5 segundos atuais cobrem
aquisição warm, refresh, sync e espera. Reservar os 12 arquivos elegíveis na
ordem original, sem cobrar arquivos não servidos/sem servidor warm como se
fossem tentativas de validação. Os restantes têm motivo explícito.

Reutilizar o paralelismo Tokio para grupos independentes; não deixar um servidor
JS silencioso consumir toda a janela antes de examinar um report Rust pronto.
Manter lease durante todo o trabalho de cada grupo, sincronizar os documentos
e ler reports já disponíveis antes de aguardar os que faltam. Não criar pool
de jobs novo nem multiplicar timeout por servidor/arquivo.

`EditDiagnosticsReport` pode continuar plano. Acrescentar identidade opcional
de servidor por `EditFileDiagnostics`; manter o campo de relatório para
compatibilidade de apresentação de um servidor e usar resumo neutro no lote
misto. Cada erro e cada motivo devem identificar sua origem quando houver
mais de um perfil. Atualizar renderer e fixtures juntos; não é necessária
uma árvore de relatórios nem um evento por arquivo.

Preservar baseline/deduplicação de erros, sanitização, redação e o limite atual
de notas. Pub sem versão, versão antiga, ausência de pub, store truncado e
baseline ausente não podem produzir a mesma afirmação de validação completa.
Um diagnóstico fresco com baseline desconhecido informa que o erro pode preexistir.

PublishDiagnostics com versão exata é o mecanismo inicial de certificação
pós-edição. Timeout, resposta malformada ou refresh falho nunca viram arquivo
limpo. Retirada de geração após mudança estrutural deixa a verificação automática
sem servidor warm; a próxima consulta explícita reabre, sem spawn escondido.

O TLS real publicou `{uri, diagnostics}` sem `version`, campo opcional do
protocolo. Nessa condição, pós-edição permanece `Unverified`, sem afirmar arquivo
limpo ou erros introduzidos; a nota usa `no verifiable diagnostics`.
Uma publicação recebida não certifica a versão do documento por si só.

Se pull entrar, guardar proveniência própria: ID do report, versão/revisão e
geração observadas. O callback síncrono do transporte não faz a consulta.
Uma request de pull deve seguir a sincronização daquele documento e validar
o estado ao responder, sem bloquear o drain de notificações. Documentos
relacionados precisam de sua própria associação; não herdam a versão do alvo.
Unchanged só reaproveita um report associado ao result ID e estado compatíveis.
Sem evidência suficiente, reportar confiabilidade reduzida.

A nota permanece antes da próxima chamada ao provider, no ponto atual de
`agent_loop`; não é uma barreira de compilação nem um check depois da
resposta final do agente. Mutação por shell/editor é observada pelo próximo
refresh, não ganha receipt de write/patch retroativamente.

Preservar a ordenação de [tool_schedule.rs](../crates/slim-core/src/runtime/tool_schedule.rs):
qualquer write/patch anterior impede antecipar uma consulta semântica. Paths ou
perfis diferentes não provam independência de imports/configs. A concorrência
interna do lote de diagnósticos acontece após as mutações e sua sincronização.

## 12. Mapa concreto de alterações

| Arquivo | Símbolos/fluxos a adaptar | O que reutilizar |
|---|---|---|
| [config.rs](../crates/slim-cli/src/config.rs) | FileLspServerConfig, LspServerConfig, merge_layer | Precedência, defaults, unknown keys e teste de disable persistente |
| [code_intel.rs da CLI](../crates/slim-cli/src/code_intel.rs) | build_code_intelligence | Uma criação por aplicação/run; sem condições exclusivas de Rust |
| [discovery.rs](../crates/slim-lsp/src/discovery.rs) | perfis, resolve_binary_from_paths/resolve_binary_on_path e entrypoint | ServerSpec e descoberta sem spawn |
| [manager.rs](../crates/slim-lsp/src/manager.rs) | resolve/discover, base_meta/degraded, cada query, notify_update, diagnostics_after_edits | OperationDocuments, staleness, contexto bounded e paginação |
| [pool.rs](../crates/slim-lsp/src/pool.rs) | chave comum, acquire/acquire_warm e start_server | ProcessFactory, leases, backoff, compartilhamento e shutdown |
| [instance.rs](../crates/slim-lsp/src/instance.rs) | handler de folders/settings, initialize, perfil do refresh e espera de diagnóstico | DocumentStore, mutex por documento, sync transaction e notification drain |
| [workspace.rs](../crates/slim-lsp/src/workspace.rs) | snapshot/project_input por perfil | FileStamp, varredura bounded e exclusões explícitas |
| [codeintel.rs do core](../crates/slim-core/src/codeintel.rs) | filtros das queries e identidade por arquivo no report | Trait, CodeIntelMeta, enum de verificação e renderer da nota |
| [tools/code_intel.rs](../crates/slim-core/src/tools/code_intel.rs) | descrição, server nas queries sem posição, status ID | Seis ações, payloads e registros existentes |
| [tools/execution.rs](../crates/slim-core/src/tools/execution.rs) | native_argument_keys e canonicalize_code_intel_arguments/allowlists | Rejeição de campos desconhecidos, paths autorizados e admissão por modo |
| [runtime/mod.rs](../crates/slim-core/src/runtime/mod.rs) | testes de workspace_tool_definitions | Cache de schemas; mudança de disponibilidade no backend |
| [native_code_intel.rs](../crates/slim-core/src/runtime/native_code_intel.rs) | compatibilidade do renderer do report | Receipt, sincronização ordenada e deadline atual |
| TUI/headless | Verificar passagem e shutdown do manager | Integração existente; sem segundo host |

Não exigir reescrever position.rs, document.rs, transporte ou a TUI para adicionar
JS/TS. Se os testes demonstrarem uma necessidade direta nesses arquivos, ajustar
somente a causa confirmada. Mantêm-se os caps atuais; tuning vem após medição.

## 13. Ordem de implementação e gates

| Etapa | Resultado concreto | Gate |
|---|---|---|
| A | Perfis/options, settings separados e chave efetiva única | Rust compatível, chaves distintas para args/settings diferentes; disable isolado |
| B | Uma consulta JS/TS real com descoberta Node/entrypoint e folders/settings | Definition/hover/diagnóstico em Windows sem TOML de ativação |
| C | Roteamento, server opcional, catálogo/status e projeções | Admissão aceita e preserva filtro até a query; projeto inferido/nested funciona |
| D | Refresh JS/TS, publicação/espera e lote misto | Módulo novo/removido e JSON estrutural atualizados; silêncio nunca limpo |
| E | Regressões, custos, docs vigentes e release | Critérios abaixo atendidos; deploy conforme RULES.md |

Começar pela integração real na etapa B antes de implementar protocolos opcionais
ou otimizar o snapshot. Descoberta/launch/handshake devem estar funcionais cedo,
para que mocks não determinem o desenho de um servidor real.

Consultas explícitas devem respeitar um deadline da operação usando o timeout
existente, incluindo resolução, inicialização, refresh, sync e RPC; não somar
timeout integral por subetapa. Preservar a inicialização compartilhada: expirar
um waiter não encerra o processo de outros callers nem deixa o pool travado.

A implementação foi autorizada após a revisão documental. Testes controlados
não substituem o gate real. Dependências ausentes
exigem instalação autorizada ou um caminho já disponível; não justificam baixar
automaticamente nem declarar integração funcional apenas com mock.

## 14. Matriz de validação

| Dimensão | Casos obrigatórios | Evidência |
|---|---|---|
| Uso nativo | Config ausente, manager criado, primeira consulta lazy; disable legado | CLI/runtime + integração real |
| Descoberta | Local antes de PATH, cache atualizado, path inválido e PATH ausente | Fixtures de filesystem sem instalação |
| Processo | Node + entrypoint, espaços/Unicode, erro de inicialização e shutdown | Stdio real e ausência de órfãos |
| Identidade | Mesmo perfil/config compartilha; args/init/settings diferentes isolam | Pool tests existentes estendidos |
| Projeto | JS/TS, JSX/TSX, ESM/CJS, .mts/.cts e .d.ts; subprojetos e inferido | TypeScript Language Server real |
| Navegação | Dois módulos, definição/referências/hover, símbolos sem documento aberto, contexto e Unicode/CRLF | Resultado real + sync controlado |
| Ferramenta | server válido/incompatível/ambíguo; canonicalização e continuidade | Core prepared path e testes de rendering |
| Frescor | Edição externa, criação/remoção, JSONC e config extends com nome arbitrário | Nova resposta real; geração/ordem nos testes controlados |
| Diagnóstico | Erro de tipo real, checkJs, publicação vazia/ausente/velha, truncamento e cancelamento | Cache/espera controlados + pub real |
| Pós-edição | Lote misto, duplicatas, 12 elegíveis, orçamento global, JS lento/Rust pronto, cold server | LSP tests + próximo request do agent_loop |
| Fronteira | Paths fora da raiz, symlink/junction, resultado externo e applyEdit recusado | Guardas atuais preservadas |
| Regressão | Rust/Cargo, limites, redação, LRU e cancelamento durante initialize | Suites pertinentes existentes |

Base de testes:
[incremental_sync](../crates/slim-lsp/tests/incremental_sync.rs),
[post_edit_diagnostics](../crates/slim-lsp/tests/post_edit_diagnostics.rs),
[mock_subprocess](../crates/slim-lsp/tests/mock_subprocess.rs),
[typescript](../crates/slim-lsp/tests/typescript.rs),
[real_typescript](../crates/slim-lsp/tests/real_typescript.rs),
[real_incremental](../crates/slim-lsp/tests/real_incremental.rs),
[adv_transport](../crates/slim-lsp/tests/adv_transport.rs),
[runtime](../crates/slim-core/src/runtime/tests.rs),
[agent_loop](../crates/slim-core/tests/agent_loop.rs) e testes nos módulos CLI/core.
Estender fixtures de stdio para capabilities/IDs JS/TS sem tornar suas respostas
prova de compatibilidade real. Preservar os testes reais Rust já opt-in.

O gate real de `workspace/symbol` como primeira consulta demonstrou a necessidade
de carregar um projeto. O manager agora abre uma única fonte do snapshot limitado,
quando disponível: a mais próxima da raiz, com desempate por path. Reutiliza
`ensure_document`, sem abrir toda a árvore. A resposta JS/TS conserva
completeness=unknown para projetos que essa abertura não comprova carregados.

O helper de diagnósticos do gate aceita `version` ausente, conforme o protocolo,
e exige completeness=unknown nesse caso. Confirma o resultado esperado em duas
leituras limitadas para não confundir uma primeira publicação vazia com o
diagnóstico semântico. Não atribui a versão local à publicação; o teste pós-edição
exige `Unverified` quando não há versão verificável, preservando o contrato.

Critérios finais: uso sem ativação, seis ações coerentes com o perfil,
sincronização e mudanças externas comprovadas, cobertura explícita em lote misto,
erro de disponibilidade honesto, ausência de mutações LSP inesperadas e nenhuma
regressão Rust causada pela mudança.

Medir cold start, consultas quentes, bytes de sync e refresh em workspace maior,
separando tempo de servidor e de snapshot. Documentar resets causados por JSON.
Não fixar promessa de latência, ganho sobre OMP ou quantidade de linhas antes
de medir. Selecionar verificações conforme o diff e
[tests/README.md](../tests/README.md); seguir [RULES.md](../RULES.md) para
revisão e publicação local quando houver implementação.

## 15. Limites atuais da validação e evolução

Na revisão documental anterior, não foram executados servidores nem testes de
código. Na implementação, os testes controlados de protocolo, subprocesso,
roteamento e disco real, a suíte workspace e a publicação local passaram.
Os gates reais [JS/TS](../crates/slim-lsp/tests/real_typescript.rs) e
[Rust](../crates/slim-lsp/tests/real_incremental.rs) também passaram, com
dependências temporárias autorizadas e sem instalação implícita pelo Slim.
Identidade do binário, versões, comandos, medições e limitações estão em
[release/README.md](../release/README.md), separados do planejamento.
Publicações TLS sem versão continuam úteis nas consultas explícitas, com
completeness=unknown, e não certificam pós-edição. Custos em projetos grandes
permanecem sem medição; tempos de fixtures não caracterizam todos os workspaces.

Python será um perfil posterior com .py/.pyi, descoberta de ambiente, settings
e inputs próprios. Preservar o roteamento/chave/report definidos aqui.
Escolher servidor Python após validação concreta; linters complementares não
substituem inteligência de tipos.

Pull/registro dinâmico, diagnóstico tardio, definição de tipo e implementação
podem seguir quando houver uso/necessidade demonstrados. Rename e code actions
exigem proposta de efeitos/conflitos/aplicação parcial. Mux requer evidência de
custo que justifique mais infraestrutura.

## 16. OMP como referência, não como obrigação

A revisão anterior do OMP foi estática e está fixada no commit citado.
Inspiram esta proposta cadastro, resolução local, roteamento e integração de
diagnósticos. Não importar o conjunto de 55 definições, todas as operações,
mux, checkers externos ou quiescence como requisito para entregar JS/TS.

Preservar no Slim sync incremental, fronteira de paths, orçamento e distinção
entre ausência de erros e ausência de validação. Testes do OMP descrevem a
cobertura de seus testes; não comprovam execução nesta sessão.

Fontes previamente examinadas:

- [Defaults](https://github.com/can1357/oh-my-pi/blob/9d8d72524fd733e38ff1af1b8c387ed01e0b1a74/packages/coding-agent/src/lsp/defaults.json)
- [Configuração](https://github.com/can1357/oh-my-pi/blob/9d8d72524fd733e38ff1af1b8c387ed01e0b1a74/packages/coding-agent/src/lsp/config.ts)
- [Cliente](https://github.com/can1357/oh-my-pi/blob/9d8d72524fd733e38ff1af1b8c387ed01e0b1a74/packages/coding-agent/src/lsp/client.ts)
- [Diagnósticos](https://github.com/can1357/oh-my-pi/blob/9d8d72524fd733e38ff1af1b8c387ed01e0b1a74/packages/coding-agent/src/lsp/diagnostics.ts)
- [Writethrough](https://github.com/can1357/oh-my-pi/blob/9d8d72524fd733e38ff1af1b8c387ed01e0b1a74/packages/coding-agent/src/lsp/writethrough.ts)
- [Testes de frescor](https://github.com/can1357/oh-my-pi/blob/9d8d72524fd733e38ff1af1b8c387ed01e0b1a74/packages/coding-agent/test/tools/lsp-diagnostics-freshness.test.ts)

Índice: [documentação do Slim](README.md). Arquitetura vigente:
[RUST-CLI.md](RUST-CLI.md). Esta proposta não substitui o contrato atual.
