# Harness Slim — etapa 5: configuração e adapters

Data: 2026-09-04. Workspace: `D:\Slim`.

## Resultado e escopo

**Verificado no código e em fixtures:** foram corrigidos limites de saída que não chegavam a ClinePass/Command Code, esforço descartado pela TUI e pelo wire Anthropic, aliases Codex enviados sem normalização, substituição silenciosa de modelo explícito, controles OpenAI Chat obsoletos, compactação que inventava parâmetros, validação Go restrita ao prompt atual e cache de catálogo Codex sem escopo no disco. xAI passou a reconhecer também o evento nativo de reasoning em texto e a validar o esforço pelo catálogo existente.

Objetivo: fazer a configuração efetiva chegar ao request correto, reutilizando os adapters e catálogos atuais. Aceitação: reproduções relevantes, requests serializados/HTTP localhost, gate integral, deploy local e documentação das lacunas. Não objetivos: chamadas pagas, migração de credenciais/modelos pessoais, novo framework de perfis, roteamento automático, completude de todos os recursos das APIs ou validação física da TUI.

O checkout já continha alterações das etapas anteriores e trabalho em xAI/OAuth. Foi preservado, sem reset, stash, checkout, commit ou limpeza. Antes das edições, fontes Rust e documentos relevantes foram copiados para `%TEMP%\slim-harness-etapa5-20260904`; `stage5.diff` nesse diretório compara esta etapa ao início da sessão. O diff contra HEAD inclui trabalho anterior e não representa autoria desta etapa. Nenhuma configuração pessoal ou credencial foi editada.

## Caminho da configuração até a rede

Leitura direta: `crates/slim-cli/src/config.rs`, `cli.rs`, `tui.rs`, `headless.rs`, `auth.rs`, catálogos CLI; `crates/slim-core/src/provider.rs`, `provider/{codex,xai,opencode_go,clinepass,command_code}.rs` e caminhos do loop/contexto/transporte pertinentes. Os símbolos abaixo identificam o contrato observado, sem depender da numeração de linhas de um checkout em movimento.

| Opção | Precedência/comportamento efetivo | Resultado da revisão |
|---|---|---|
| Modelo e endpoint | CLI > ambiente > TOML do projeto > TOML global > default do provider | Preservado. Modelo explícito incompatível agora dá erro; default de arquivo incompatível ainda permite fallback entre providers conforme decisão G248 existente. Alias Codex é canonicalizado antes do envio. |
| Provider | CLI/`SLIM_PROVIDER`; TUI pode restaurar autenticação ativa | Não existe chave `provider` no schema TOML atual. Headless exige ativação explícita de provider; configuração de arquivo sozinha não liga rede. |
| Projeto/global | `./slim.toml` no cwd; global APPDATA ou `SLIM_CONFIG_FILE` | `load_layered` mescla global e projeto. Arquivo presente inválido é erro. Chaves desconhecidas são ignoradas pelo contrato atual. Não é busca ascendente por raiz Git. |
| Limites numéricos | Opção programática > ambiente > arquivo > default | `file_default` já evita que TOML sobreponha ambiente; a suspeita inicial de precedência invertida foi descartada após ler esse caminho. Variável numérica inválida não vira fallback silencioso. Não foi necessário alterar `config.rs`. |
| Esforço | `SLIM_EFFORT` não vazio > `effort` TOML; opção programática encaminhada ao executor | TUI agora rejeita valor não reconhecido e encaminha esforço explícito também para Anthropic e IDs compatíveis livres. Não há nova flag de esforço. Ausência continua delegando ao default do provider. |
| Contexto | Opção programática/`SLIM_CONTEXT_WINDOW_TOKENS` > metadado conhecido > fallback do runtime | Não existe chave TOML de contexto. Default genérico é 32.000; Go valida override contra seu metadado. Catálogo Codex em cache participa quando não há override e o endpoint é elegível. |
| Saída | Opção programática/`SLIM_MAX_OUTPUT_TOKENS`/TOML; default 4096 | ClinePass e Command Code agora recebem o valor resolvido. Codex assinatura usa reserva local, sem campo de teto no payload. Contexto e teto de geração não são a mesma coisa. |
| Credencial | Resolver próprio: chave genérica/específica de env e auth persistida; OAuth tipado onde implementado | Preservado. Tokens não foram impressos nem usados contra providers. TUI Codex explícito segue OAuth ativo; o caminho headless admite a credencial configurada no seu resolver. Não foi feita migração de autenticação. |

Headless e bridge TUI chegam a `execute_provider_turn_async`, que resolve budgets, monta o adapter e chama o mesmo agent loop com mensagens. A TUI mantém interação e Plan com tools de leitura; headless Plan exige aprovação conforme contrato existente. A retomada durável alimenta mensagens/checkpoint no mesmo executor; `--recover` é recuperação local e não um segundo adapter de inferência.

Limite remanescente de UI: `ReasoningEffort` oferece low/medium/high/xhigh/max/ultra. Valores nativos como none/minimal não estão representados na TUI; headless compatível aceita strings de esforço. O indicador inicial High sem seleção explícita não prova que High foi enviado. Não se adicionou um default global novo para mascarar essa diferença.

## Matriz de rotas existentes

**Verificado localmente:** sete providers lógicos, três protocolos. API compatível descreve o formato HTTP; não prova equivalência de capacidades, autenticação, disponibilidade ou cobrança com a API do fabricante.

| Rota Slim | Endpoint/wire e autenticação existentes | Modelo, controles e limites |
|---|---|---|
| OpenAI-compatible | Endpoint completo; padrão `https://api.openai.com/v1/chat/completions`; Bearer | IDs livres. No host oficial usa `max_completion_tokens` e pede usage final. Gateways conservam `max_tokens`. Esforço selecionado em `reasoning_effort`; suporte efetivo depende do modelo/servidor. |
| Codex assinatura | Backend ChatGPT, `/codex/responses`; Bearer OAuth, account-id e headers específicos | Catálogo/aliases Codex, `reasoning.effort`, summary auto, `store:false`, encrypted reasoning e tools Responses. Reserva de saída local; nenhum `max_output_tokens` inventado na compactação. Não é a API pública OpenAI cobrada por chave. |
| xAI | `https://api.x.ai/v1/responses`; chave ou token OAuth em Bearer, sem headers Codex | Quatro modelos no registro existente. Contexto/saída e níveis de esforço por modelo; Responses com teto de saída. Validação de esforço agora no adapter; eventos reasoning text e summary aceitos. |
| Anthropic | `/v1/messages`; chave `x-api-key` ou variante OAuth com Bearer/betas | `max_tokens`; esforço explícito em `output_config.effort`. Não liga extended/adaptive thinking automaticamente. IDs livres; sintaxe do esforço validada, suporte por versão de modelo continua dependente do servidor. |
| OpenCode Go | `https://opencode.ai/zen/go/v1`; Bearer de assinatura/gateway | Registro fechado de 24 modelos escolhe Chat, Responses ou Messages. Contexto/saída/imagem/esforço existentes são preservados; imagem agora validada em todo histórico, inclusive prepared requests. Messages de MiniMax/Qwen permanece sem parâmetro Anthropic de esforço: seu catálogo só oferece high e não há contrato confirmado de controle ajustável nessa rota. |
| ClinePass | `https://api.cline.bot/api/v1/chat/completions`; Bearer | IDs `cline-pass/*`, catálogo embutido e dinâmico já existentes; reutiliza Chat. Agora recebe teto de saída resolvido. Não converte prefixos de assinatura em IDs públicos de outra oferta Cline. |
| Command Code | `https://api.commandcode.ai/provider/v1`; Bearer | IDs Claude usam Messages; outros usam Chat, conforme seleção existente. IDs dinâmicos sintaticamente válidos continuam aceitos. Agora teto de saída nos dois wires e esforço em Messages. |

Gemini, DeepSeek, GLM, Kimi, MiniMax, Qwen, Muse e outras famílias acessíveis pelos gateways continuam usando a rota do gateway. Não há adapter direto Gemini/Bedrock/Vertex no conjunto revisado; não foram criados como consequência de nomes no catálogo.

## Correções e evidência específica

1. **Teto de saída perdido nos wrappers.** `ClinePassAdapter::with_max_output_tokens` e `CommandCodeAdapter::with_max_output_tokens` atualizam o config já existente; o executor os chama. A fixture de CLI captura o HTTP ClinePass com `max_tokens:1234`; fixtures Command Code conferem esse valor em Chat e Messages. Não há segundo config de geração.

2. **Esforço Anthropic e TUI descartado.** `AnthropicAdapter::messages_body` emite `output_config.effort`; executor e Command Code encaminham a escolha. A TUI retirou a allowlist paralela que excluía Anthropic e modelos livres. Teste verifica medium sem campo `thinking`; fixture de ambiente TUI verifica esforço explícito e erro para valor inválido. A validação Anthropic é sintática: não foi inventada uma tabela de recursos por todas as versões Claude.

3. **OpenAI Chat oficial.** O builder de mensagens usa `max_completion_tokens`, incluindo orçamento de reasoning, e `stream_options.include_usage`. `build_request` delega ao builder de mensagens; removida a montagem duplicada. A fixture verifica formato oficial e igualdade entre builders. A condição usa o helper de host oficial já existente; não impõe o campo novo aos gateways.

4. **Compactação que alterava o contrato do modelo.** `harden_compaction_body` deixou de forçar low e de inserir teto inexistente. Mantém esforço selecionado, retira tools e reduz apenas campos de saída já presentes a no máximo 2048. Fixtures demonstram esforço high preservado, ausência de teto Codex e prepared compaction Go high-only. Finalização conserva a política anterior: low apenas quando o catálogo Go/xAI comprova suporte; limite menor nunca é aumentado.

5. **Modelo explícito e alias.** CLI e TUI distinguem modelo explícito de default de arquivo. `canonical_provider_model` consulta o catálogo existente; `OpenAiCodexAdapter::new` também normaliza para chamadas diretas. Fixture envia `terra` e exige `gpt-5.6-terra` no body. Não muda preferência salva, nem aceita trocar silenciosamente um modelo explicitamente incompatível por outro.

6. **Contrato Go no local que prepara requests.** `validate_messages` centraliza normalização e veto de imagem para modelos só de texto. É usado no preflight da CLI e em builders normais/prepared/compactação. O primeiro gate integral detectou que mover tudo só para o adapter mudava o erro público para resultado de falha de provider; a chamada de preflight reutiliza a mesma validação e preserva o erro de entrada. O teste existente `text_only_model_rejects_images_before_network` voltou a passar sem ser relaxado. Esforço também é validado no constructor, eliminando a checagem duplicada no headless.

7. **xAI.** O adapter valida níveis pelo catálogo e traduz `response.reasoning_text.delta` para o evento já existente. Teste cobre xhigh de Grok 4.6, recusa ultra em 4.5 e parsing do delta. Não usa metadado Codex para decidir capacidades xAI.

8. **Catálogo Codex no disco.** A memória já separava conta/endpoint; o JSON persistido não. Agora ambos usam digest SHA-256 de endpoint + separador + account-id, com dependência já presente. Cache sem scope ou de outro scope vira miss e usa fallback existente. Fixture verifica isolamento por conta e endpoint. Não grava token, account-id bruto ou endpoint bruto no novo campo. Arquivo legado não recebe camada de migração nem é apagado nesta tarefa.

## Catálogos, capacidades, ferramentas, cache e continuidade

**Verificado:** Go usa lista remota filtrada pelo registro de metadados conhecido. ClinePass/Command Code têm descoberta dinâmica na UI, mas a execução ainda obtém contexto/saída principalmente do registro embutido; modelo desconhecido cai no fallback. Anthropic reutiliza contexto de entradas Claude do catálogo Command Code quando há correspondência. Esse reaproveitamento não prova que limites de uma oferta gateway valem na API oficial. Não foi criada uma segunda matriz para duplicar os mesmos dados incompletos.

O catálogo Codex usa cache em memória com TTL e disco como fallback. O novo scope resolve mistura entre contas/endpoints, mas o arquivo persistido continua sem validade temporal própria. Refresh em background não é garantia de catálogo fresco no primeiro request. Os catálogos públicos Go/Command Code usados pela UI também não comprovam metadados de um endpoint customizado. A documentação Go consultada já lista IDs além dos 24 locais; a lista embutida é um subconjunto suportado pelo Slim, não inventário atual completo do serviço.

`ProviderCapabilities` descreve materialização de cache, não todos os recursos de um modelo. Mantê-lo com esse escopo evita confundir suporte a cache com visão, esforço ou contexto. Os wrappers delegam conforme o wire. A validação Go passou ao adapter; não foi adicionada uma autoridade paralela de capacidades no CLI.

Tools continuam saindo do registro normalizado existente; Chat usa function calls, Responses usa itens/call-id, Messages usa tool_use/tool_result. Os filtros de modo e orçamento permanecem no loop. Compactação/finalização removem tools. Não foram habilitadas ferramentas hospedadas como web search, execução remota do fabricante ou novas ferramentas com efeito externo.

Imagens locais mantêm PNG/JPEG/GIF/WebP, arquivo regular sem symlink e limite de 20 MiB. O request multimodal é serializado pelo wire, e Go agora verifica também imagens antigas no histórico. Não há alegação de suporte geral a áudio, PDF ou arquivos: blocos não implementados não se tornam entrada nativa por serem aceitos no tipo intermediário. Outros providers ainda dependem de metadado incompleto/validação do servidor para certos modelos sem visão. Estimativa local de contexto não equivale a tokenização de imagens ou reasoning cifrado.

Cache nativo de prompt permanece distinto de cache local de resposta. `with_shared_transport` passa `None` para o cache local: replay de respostas permanece desligado no produto. Foram corrigidas afirmações antigas em dois documentos de status que ainda o descreviam como ativo. O transporte preserva namespace, escopo e materialização já implementados; os novos campos semânticos fazem parte do request e de sua identidade existente. Não foi adicionado cache entre credenciais nem replay de tools. Breakpoints Anthropic e intent de cache Responses/Chat não comprovam hit, desconto ou retenção no serviço; isso exige usage live.

A continuidade das etapas 3–4 foi preservada: argumentos completos, call IDs, resultados de tools e itens Responses de reasoning opaco conservados dentro da mesma instância de adapter. A captura opaca continua dependente de `output_item.done`; não se adicionou fallback pelo agregado final. Nova instância, novo turno TUI/restart e retomada durável não ganham automaticamente estado opaco anterior. Blocos Anthropic `thinking`/`signature`/`redacted_thinking` e detalhes opacos de reasoning dos gateways Chat ainda não possuem replay completo. Não habilitar thinking automaticamente evita introduzir um protocolo que o histórico ainda não conserva integralmente.

Requests de recuperação após erro/compactação passam pelos mesmos adapters e configuração resolvida. Permanecem os limites da etapa 4: término nativo sem esperar EOF em Responses/Messages, Chat esperando o terminal de usage/DONE, timeouts pós-envio sem retry cego, usage parcial preservado e cancelamento. Não foi reaberta a decisão de não reiniciar tools automaticamente.

## Documentação oficial consultada

Consultas públicas, sem usar credenciais e sem gerar inferência. Documentação é evidência do contrato publicado, não de sucesso live do Slim.

| Fonte | Confirmação usada e limite |
|---|---|
| [OpenAI Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) | `max_completion_tokens` substitui o teto antigo, inclui reasoning; opt-in de usage no streaming. Aplicação limitada ao endpoint oficial reconhecido. |
| [Anthropic effort](https://platform.claude.com/docs/en/build-with-claude/effort) | `output_config.effort` funciona com ou sem thinking. Níveis disponíveis dependem do modelo; não equivale a habilitar adaptive thinking. |
| [Claude Code autenticação](https://code.claude.com/docs/en/authentication) e [condições de uso](https://code.claude.com/docs/en/legal-and-compliance) | Assinatura/OAuth não é contrato intercambiável com chave da API. A rota OAuth já existente no Slim foi preservada, sem afirmar suporte oficial de terceiros ou autorização live. |
| [xAI reasoning](https://docs.x.ai/developers/model-capabilities/text/reasoning) | Esforço em Responses e eventos de reasoning; Grok 4.6 oferece xhigh. Sucesso da autenticação/device flow e disponibilidade por conta não foram testados. |
| [OpenCode Go endpoints](https://opencode.ai/docs/go/#endpoints) | O próprio gateway publica Chat, Responses e Messages por família. Catálogo público atual é maior que o registro Slim; ausência de ID antigo na página não foi tratada como prova de remoção do serviço. |
| [Command Code Provider API](https://commandcode.ai/docs/provider) | Endpoints Chat e Messages, Bearer, Claude na rota Messages e usage terminal. O schema publicado justifica preservar controles nativos de cada wire; aceitação de esforço por todo modelo permanece sem prova live. |
| [Cline API](https://docs.cline.bot/api/overview) e [Chat Completions](https://docs.cline.bot/api/chat-completions) | Confirmam host e protocolo público compatível, tools e reasoning em delta. A referência consultada não confirma integralmente o catálogo/prefixo ClinePass nem seus controles de esforço/teto; não se inferiu equivalência entre oferta pública e assinatura. |

Não foi necessário tratar a API pública Responses da OpenAI como documentação do endpoint interno Codex. O contrato de assinatura previamente verificado por fixtures foi mantido; a mudança remove o campo contraditório que só aparecia na compactação.

## Validação desta sessão

Reproduções anteriores ao patch: `provider_adapters` teve 30 passed/3 failed (alias Codex, esforço Anthropic ausente, campo OpenAI); filtro de compactação em `provider_http`, 0 passed/2 failed (low forçado e teto Codex inventado); fixture de cache Codex, 0 passed/1 failed (conta errada aceita). Logs `red-core.log`, `red-compaction.log`, `red-catalog.log` no diretório temporário da etapa. Esses resultados são RED intencional, não o gate final.

Fixtures existentes foram estendidas para os wrappers/CLI/TUI; quatro testes pequenos foram acrescentados para comportamentos sem cobertura suficiente. Nenhum framework, dependência, snapshot grande ou diretório de testes novo. Asserções verificam payload/efeito observável; localhost captura o request real do caminho CLI ClinePass. Os demais adapters possuem fixtures próprias existentes incluídas no gate.

O primeiro gate integral encontrou a regressão de preflight Go descrita acima. Foi corrigida na implementação mantendo o teste original. Resultado final medido: **1107 passed / 0 failed / 1 ignored (ConPTY físico), em 88 suítes, zero compiler warnings**, por `cargo test --workspace` dentro de `refresh-slim.ps1 -Test`, sem filtros. Logs temporários preservados para inspeção local.

| Verificação | Saída observada |
|---|---|
| `refresh-slim.ps1 -Test` / `cargo test --workspace` | EXIT=0; 1107 passed, 0 failed, 1 ignored, 88 suítes; zero warnings. |
| `cargo clippy --workspace --all-targets -- -D warnings` | EXIT=0 depois da correção do preflight Go. |
| `rustfmt --edition 2021 --config skip_children=true --check` nos 18 arquivos Rust da etapa | EXIT=0. |
| `cargo fmt --all -- --check` | EXIT=1: drift em arquivos inalterados desde a cópia inicial, incluindo OAuth, TUI e LSP. Nenhuma formatação global aplicada. |
| `git -c core.safecrlf=false diff --check` | EXIT=0. |
| Executável instalado `--version` e `--headless --fake --prompt 'stage5 offline smoke'` | `slim 0.1.0` / `success`; ambos EXIT=0. |

Saída do deploy:

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 20:56:45)
```

`target/release/slim.exe` e `C:\Users\User\bin\Slim.exe`: **15.011.840 bytes**, SHA-256 idêntico `363882A70F8AEE420012EF85519B22072BFF20D2DB97011929B45571CE0FA30C`. O hash e os smokes foram lidos nesta sessão. Logs finais: `refresh-final.log`, `clippy-final.log`, `fmt-touched.log`, `fmt-global.log` no diretório temporário da etapa.


Não houve chamada paga ou inferência live, nem teste de login/refresh com contas reais. Fixtures não provam latência/custo, disponibilidade, cache hit, limites contratados, aceitação de todos os parâmetros em cada gateway, quotas ou qualidade de cada modelo. ConPTY físico e a matriz de terminal permanecem sem validação. O ZIP de distribuição não foi regenerado.

## Síntese das cinco etapas

Os registros [1](HARNESS-SLIM-ETAPA-1.md), [2](HARNESS-SLIM-ETAPA-2.md), [3](HARNESS-SLIM-ETAPA-3.md) e [4](HARNESS-SLIM-ETAPA-4.md) foram consultados. Seus resultados pontuais continuam históricos; o gate desta etapa reexecutou a suíte do checkout resultante.

| Etapa | Melhoria comprovada por regressões/fixtures | Complexidade removida ou contida |
|---|---|---|
| 1 — simplicidade interna | Descarte de future preserva estado no AppHandle; execução local/LSP comum; indicador tok/s usa relógio do runtime, com estimativa explícita. | Removidos montagem redundante de limites, clone evitável de histórico e telemetria exclusiva de teste; sem segundo runtime. |
| 2 — loop e ferramentas | Falha de shell chega como falha; conclusão de lote conserva resultados; budgets respeitam prefixo/barreiras; repetição considera mudança de estado; cancelamento permanece cancelamento. | Menos regras baseadas só em igualdade de texto; reaproveitamento do governor/loop, sem classificador LLM de progresso. |
| 3 — contexto e histórico | Argumentos e reasoning Responses opaco preservados no ciclo suportado; pedido recente e autoridade conservados; checkpoint único e recuperação por artefato. | Removida duplicação de checkpoint; sem sistema paralelo de memória ou replay especulativo de operações. |
| 4 — transporte | Término nativo, retry conservador após envio, SSE incremental e usage observado sobrevivendo a falhas/cancelamento. | Evita aguardar EOF sem necessidade e revarrer prefixos; mantém transporte compartilhado. |
| 5 — configuração/providers | Escolhas chegam aos respectivos campos; aliases e erros explícitos coerentes; compactação respeita contrato; cache Codex isolado; Go/xAI validam no adapter. | Builder Chat único; removida allowlist paralela de esforço da TUI e checagem duplicada Go; nenhuma camada de perfis ou seleção automática. |

O conjunto melhora fidelidade e continuidade do harness em testes locais. Não comprova superioridade de qualidade, custo ou velocidade de modelos em produção. A etapa 5 acrescenta controles e validações pequenos; não é alegação de redução líquida de linhas de todo o checkout.

Limitações restantes prioritárias: catálogo/UI/runtime ainda não compartilham todos os metadados dinâmicos; TUI não representa todos os níveis nativos; aliases/IDs fora de catálogo e gateways dependem do servidor; tokenização multimodal é estimativa; replay opaco entre instâncias/durable resume e thinking Anthropic não está completo; credenciais, quotas e desempenho live não foram exercitados. São lacunas delimitadas, sem proposta automática de uma sexta etapa.

Propostas descartadas nesta revisão: perfis por modelo e roteador automático; trocar defaults pessoais pelo modelo mais recente; unificar todo esforço em low na compactação; enviar controles OpenAI oficiais para qualquer host; habilitar thinking sem replay; preencher capacidades de novos IDs pela semelhança do nome; copiar catálogos completos sem contexto/saída/modalidades verificáveis; migrar cache legado sem conhecer seu escopo; substituir decisões anteriores sem nova reprodução; ampliar infraestrutura de testes ou executar chamadas pagas para parecer completo.

## ✅ Verificação de Entrega — RULES.md §4

- [x] Rodei o que afirmo ter rodado: comandos e saídas registrados acima.
- [x] Números contados nesta sessão: 1107 passed, 0 failed, 1 ignored, 88 suítes.
- [x] Código e símbolos citados lidos nesta sessão; referências locais por arquivo/símbolo, sem linhas presumidas.
- [x] `cargo test --workspace` verde: 0 failed, 0 compiler warnings.
- [x] `.\refresh-slim.ps1 -Test` executado, imprimiu `OK:` reproduzido acima.
- [x] Índices/status atualizados; busca dos totais/hashes anteriores vazia nas seções de estado atual. Linhas históricas do tracker e relatórios 1–4 mantidas deliberadamente, não reescritas como resultado desta etapa.
- [x] Incertezas e limitações declaradas. Validação live, pagamento, ConPTY físico, ZIP e mudança visual: não aplicáveis ao escopo executado, sem alegação de sucesso.

