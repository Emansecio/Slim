# Histórico de mudanças do Slim

> Registros preservados do README até 20/09/2026. São históricos e não
> substituem o estado vigente do checkout, o código ou `release/README.md`.

**Checkout — decisão econômica Jev e contexto entre lotes, 20/09/2026:**
o pré-passe Jev compara seu custo estimado com um teto otimista da economia
possível na entrada do resumo, usando o preço de entrada do provedor configurado
por `SLIM_INPUT_COST_MICROS_PER_MILLION` e o preço TypeSafe padrão. Se nem esse
teto paga Jev, o resumo convencional continua; preço desconhecido não produz
uma comparação fictícia. Cada lote recebe corpos completos dos seus candidatos
e trechos limitados dos demais, identificados pela ordem global. A instrução
preserva candidatos quando a decisão depende de evidência omitida ou truncada.

**Histórico — compactação Jev e checkpoint endurecidos, 20/09/2026:** cada
`tool_call_id` seguramente ligado a um único resultado é um candidato
independente. Cada lote recebe somente os corpos completos dos seus candidatos
e um índice cronológico compacto do conjunto global; conteúdo e blocos do
assistant, chamadas irmãs, IDs e nomes permanecem intactos. Quando uma chamada
é podada, apenas seus argumentos viram `{}` e o resultado ligado recebe uma
nota menor. Candidatos ambíguos, resultados com blocos estruturados, conjuntos
acima de 256 itens e reduções insuficientes permanecem verbatim e o resumo LLM
continua disponível.

TypeSafe usa `POST /v1/systemone`; Vercel usa a rota oficial `POST
/v1/evaluate`, ambas com `model` no corpo e contratos próprios de resposta.
IDs, tipo e probabilidades precisam corresponder exatamente e valores fora de
`[0,1]` ou não finitos são rejeitados sem clamp. Os limites locais em caracteres
são apenas guardas de construção — os limites em tokens documentados pelo
provider continuam autoritativos. Há deadline total de 90 s, timeout por
request de 30 s e cancelamento cooperativo em foreground/background. Eventos
separam lotes iniciados/concluídos, uso confirmado/desconhecido, backend e
modelo solicitado/resolvido.

O break-even do resumo não soma tokens de modelos com preços diferentes. Custo TypeSafe Jev
conhecido usa US$ 0,042/M de tokens de entrada e saída gratuita; Vercel/modelos
customizados permanecem explicitamente sem preço estático. O checkpoint final
é revalidado depois de fatos, recuperação e manifesto (máximo 4 KiB), conserva
os sete títulos estruturais e fronteiras UTF-8 e falha explicitamente quando os
metadados mínimos não cabem. A notificação Jev informa que a poda terminou, mas
que o checkpoint ainda não foi aplicado.

**Histórico — introdução da poda Jev, 18/09/2026:** a compactação passou a ter
duas estratégias selecionáveis. `jev` é o padrão: antes de pedir o resumo ao
modelo principal, o Slim consulta o Jev (TypeSafe) para julgar quais pares
tool call/result do prefixo resumido ainda importam; os obsoletos são
descartados e o restante permanece verbatim — nada é reescrito por um modelo
generativo. O prefixo já podado segue para o mesmo resumo LLM de antes, então
o checkpoint, o fingerprint, a telemetria e os eventos continuam idênticos.
`summary` mantém exatamente o comportamento anterior. A troca é explícita:
`--compactor jev|summary`, `SLIM_COMPACTOR` ou `[compaction] strategy` no
`slim.toml`, com precedência CLI > env > arquivo. Dois endpoints servem o
mesmo modelo: a API System One da TypeSafe (`TYPESAFE_API_KEY`, padrão
`jev-1.13.0`) e a rota de avaliação do Vercel AI Gateway
(`AI_GATEWAY_API_KEY`, padrão `typesafe-ai/jev`); `SLIM_JEV_BACKEND`
(`typesafe`|`vercel`) escolhe explicitamente e `SLIM_JEV_MODEL` sobrepõe o
modelo. Sem credencial o modo não faz chamada alguma. O enforcement é
fail-open com aviso: falha de rede, resposta inválida, poda insuficiente ou
credencial ausente emitem `CompactionJevFallback` com o motivo e a
compactação segue pelo resumo LLM. Sucesso emite `CompactionJevPruned` com
pares podados, resultados truncados, lotes e tokens estimados; a TUI mostra
ambos como notificação. Um par só é podado quando a nota substitui conteúdo
de fato maior (mínimo de 512 caracteres), para nunca inflar o contexto.
Testes: unitários do pruner com judge fake (sem rede) e dois testes de loop
no `agent_loop`, um de poda com arquivo grande e outro de fallback.

**Histórico — backend Vercel do Jev, 19/09/2026:** sem `SLIM_JEV_BACKEND`, a
própria chave decide o endpoint: `vck_` é o prefixo que a Vercel emite, então
uma chave do gateway basta e uma `TYPESAFE_API_KEY` comum continua na TypeSafe
(uma chave `vck_` guardada em `TYPESAFE_API_KEY` também chega ao gateway). O
contrato do gateway diferia do TypeSafe em dois pontos, tratados no mesmo
módulo: o primitivo booleano chama-se
`boolean`, com a probabilidade em `probability` em vez de `noul`. Validação
desta sessão: `cargo check --workspace --all-targets` limpo, `cargo fmt --all`
aplicado, `slim-core --lib jev` 9/9, `slim-cli --lib config` 26/26 e
`agent_loop jev` 2/2. Chamada viva ao gateway desta conta respondeu
`customer_verification_required` ("requires a valid credit card on file"):
chave e rota estão corretas, mas a conta Vercel precisa cadastrar cartão para
liberar os créditos gratuitos. O backend TypeSafe está validado ao vivo nesta
sessão: `POST https://api.typesafe.ai/v1/systemone` respondeu 200 com
`model: jev-1.13.0`, e o `HttpJevJudge` real (reqwest + parse) podou dois pares
com `estimated_saved_tokens: 4340`. São chamadas reais, que consomem tokens da
chave TypeSafe; nenhum benchmark pago foi executado. Publicado no PATH em
19/09/2026 — [identidade do build e smoke](release/README.md).

**Histórico — remoção do modo Jev, 18/09/2026:** o modo Jev (roteamento de
tool call) foi removido. O ciclo de modos voltou a
`Auto → Read-only → Plan → Auto`; `Auto`, `Read-only` e `Plan` não mudaram.
Saíram junto o controlador TypeSafe (`/login typesafe`), a flag `--jev`, os
eventos e fatos duráveis `jev.*`, o custo em categoria própria
(`RequestKind::JevDecision`) e toda a maquinaria de reasoning OFF do provider
(`ReasoningOff`, `reasoning_off_support`, `SLIM_JEV_GATEWAY_OFF` e o
`tool_choice: required`), que só existia para servir o modo. O seletor
`/model` voltou a listar todas as rotas, sem filtro de compatibilidade, e
nenhum modo impõe esforço. `--effort none` continua enviando o toggle nativo
(`thinking: {"type": "disabled"}`) em DeepSeek/GLM/Kimi. A credencial TypeSafe
eventualmente gravada em `~/.slim/auth.json` é preservada apenas para que
arquivos existentes continuem sendo lidos; o Slim não a usa nem a reescreve. O
modo não chegou a ser homologado, e a proposta que o descrevia
(`PROPOSTA-MODO-JEV-SLIM-2026-09-17.md`) e o bench live (`bench/jev-live/`)
foram removidos.

Cada operação durável continua gravando `run.telemetry.v1`: um snapshot
sincronizado antes do primeiro efeito e outro no mesmo lote do término/falha,
com modo, provider/modelos, revisão, IDs opcionais de experimento/tarefa,
uso/custos agregados, validação e limites resolvidos. O reducer expõe o snapshot
terminal por operação, enquanto o JSONL mantém ambos e continua legível por
consumidores anteriores. Benchmarks podem definir os IDs sem contaminar o prompt
usando `--experiment-id ID` e `--task-id ID` (ou os builders equivalentes de
`ProviderRunOptions`).

Validação desta sessão: `cargo check --workspace --all-targets` sem avisos,
`cargo fmt --all -- --check` e `git diff --check` limpos. No `cargo clippy
--workspace --all-targets` restam duas falhas pré-existentes, em arquivos não
tocados por esta remoção (`layout_golden.rs` e `auth.rs`); permitindo essas duas
regras, o restante passa. Smoke real: `--help` sem `--jev`, `--jev` rejeitado
como opção desconhecida e `--headless --fake` com exit 0. A suíte `cargo test`
não foi reexecutada nesta tarefa — o recorde anterior continua sendo histórico.
Nenhuma chamada a provider comercial foi feita.

Validação da compactação por poda Jev (mesma sessão): `cargo check --workspace
--all-targets` sem avisos, `cargo fmt --all -- --check` e `git diff --check`
limpos, e `cargo clippy -p slim-core --all-targets -D warnings` limpo. Em
`slim-cli` o clippy fica nas mesmas duas falhas pré-existentes já citadas
(`layout_golden.rs` e `auth.rs`), ambas fora do escopo desta tarefa. Suítes:
`slim-core` **57 suítes / 905 passed / 0 failed**, `slim-cli` **24 suítes / 314
passed / 0 failed** (inclui o golden do `--help` e o novo
`compactor_flag_rejects_unknown_name_and_accepts_known_ones`). Em `slim-tui`
permanecem duas falhas pré-existentes da família "overlay de modelos/OpenCode"
(`reducer::opencode_models_refresh_and_select_dynamic_catalog` no lib e
`model_overlay_golden::opencode_catalog_filters_and_enter_sends_selection` na
integração), em `reducer.rs`/`app.rs`, arquivos já modificados no working tree
antes desta tarefa e que não passam pelo caminho alterado (`from_core` só ganhou
dois braços). O restante passa: 252 no lib e 232 na integração. Testes novos
desta tarefa: 4 unitários do pruner com judge fake (poda com preservação de ids,
preservação verbatim quando o judge mantém, lista curta de respostas e ausência
de candidatos) e 2 de loop (`agent_loop::jev_*`) — poda real com par antigo
grande e fallback com judge falhando. Nenhuma chamada à TypeSafe foi feita nos
testes: o judge é injetado e os fixtures são localhost.

A estratégia governa também a compactação em background: o plano de background
poda o prefixo antes de montar o prompt, e os eventos `CompactionJevPruned` /
`CompactionJevFallback` saem no mesmo ponto em que a tentativa realmente começa
(`CompactionAttemptStarted`). Quando o judge não está configurado, o caminho cai
no resumo LLM sem chamada externa.

**Checkout — auditoria de performance e corretude, 16/09/2026:** varredura
paralela dos quatro crates aplicou otimizações de comportamento idêntico no
slim-core (estimador de tokens por byte-scan, fast-path sem segredos na redação
e no clone de histórico, framer MCP O(n), contagem de bytes no body) e na
slim-tui (decoder de paste O(n²)→O(n), scrollbar sem alocação por frame,
scans únicos no reducer), além de dois bugs corrigidos no slim-cli (stdin
ignorado com `--effort`/`--fast`/`--normal`/`--abandon-pending`; leitura de
header de sessão sem limite, agora 64 KiB) e um bug latente no cache
ponderado da TUI. A suíte perdeu 4 testes inúteis e 1 fixture órfão.
Medição dedicada (`bench/gauge-prepare-cost`) localizou o custo dominante por
turno na serialização dupla do accounting de prepare (~20 ms @1 MiB de body) —
registrada como proposta, pois exige mudança no contrato `ProviderAdapter`.
Validação desta sessão: `cargo fmt --all -- --check` limpo,
`cargo clippy --workspace --lib --bins --offline -- -D warnings` limpo e
`cargo test --workspace --offline` com 1673 testes aprovados, 0 falhas,
34 ignorados. Apenas checkout, sem deploy; números de bench em profile dev.

**Checkout — foco nos campos de texto, 16/09/2026:** login por API key, busca,
paleta e filtro de modelos passam a posicionar o cursor nativo no campo ativo.
A barra pisca pelo próprio terminal, sem timer extra, e sua forma padrão é
restaurada ao sair. O login mantém a chave mascarada, mostra orientação quando
vazio e exibe progresso/erro de salvamento; durante a operação, oculta o cursor.
Apenas checkout, sem deploy; piscada física depende do suporte do terminal.

**Checkout — catálogo ClinePass, 16/09/2026:** o limite de 256 modelos é aplicado
depois do filtro `cline-pass/`, evitando rejeitar um catálogo geral grande que
contenha modelos elegíveis. O limite bruto de 1 MiB permanece. Na consulta desta
data, Union Alpha apareceu nos endpoints Go/Zen (`union-alpha`) e Cline
(`stealth/union-alpha`), mas os registros locais Go/Zen e o namespace ClinePass
não o admitem. O GET Cline autenticado retornou 444 IDs, nenhum `cline-pass/`;
nesse caso, o Slim usa o bundle estático. Não foi adicionada exceção para o modelo
nem validada inferência com ele. Apenas checkout, sem deploy.

Validação dessas duas correções: `cargo test -p slim-tui --offline` (505 testes
aprovados, 5 ignorados), teste `clinepass_provider` (6 aprovados), catálogos
Go/Zen e smoke de integração (17 aprovados). `cargo fmt --all -- --check` e
`cargo clippy --workspace --lib --bins --offline -- -D warnings` passaram.
A piscada não foi observada em console físico nesta validação.

**Checkout — limpeza de APIs, 16/09/2026:** removidos wrappers sem consumidores,
helpers visuais obsoletos, snapshots artificiais de smoke, o scheduler legado e
os módulos isolados de snapshot/watch/hooks/telemetria. O smoke usa estado,
eventos e renderização reais da TUI. O journal, a persistência de TODO e os
contratos duráveis de child/Plan/Goal permanecem; os registros `task.v1` continuam
aceitos na retomada. Esta limpeza reduz APIs públicas e não é compatível com
consumidores externos das APIs removidas. Apenas checkout, sem deploy.
[Relatório da limpeza, medições e validação](analysis_outputs/LIMPEZA-2026-09-16.md).

**Checkout — composer, colagem e fila, 16/09/2026:** a colagem do Windows passa
por um decodificador no fluxo de eventos (`input.rs`): marcadores `ESC [200~` /
`ESC [201~` viram um payload atômico e um paste sem marcadores tem os `Enter`
convertidos em texto, então colar várias linhas nunca envia o prompt. As
posições da fila começam em 1 e o composer informa a ação de Enter. Este é o
estado do checkout; [validação](docs/AUDIT-SLIM-TUI-TRACKER.md)
registra os gates.

**Checkout — feedback e leitura da TUI, 15/09/2026:** atividade acompanha as
chamadas paralelas por identidade; preparação, admissão, execução, retry e
cancelamento têm estados distintos. A fila fica pausada após interrupção
solicitada e oferece `/queue status|pause|resume|edit N|remove N`. O pensamento
usa uma prévia incremental de duas linhas; seleção, aprovações, TODO,
notificações e encerramento têm comportamento explícito. Coluna central de
100 células (workspace de até 144 com inspetor), interface em português e
ênfases breves respeitam movimento reduzido. [Contrato](docs/DESIGN-SLIM-TUI.md#12-direção-visual-revisada),
[validação](docs/AUDIT-SLIM-TUI-TRACKER.md) e
[registro de deploy](release/README.md).
Instalado no PATH em 15/09 às 22:00, com hashes release/PATH iguais e smoke
de inicialização, paleta e encerramento da TUI em PTY aprovado.

**Checkout — ferramentas e contexto, 15/09/2026:** descrições nativas mais
curtas, preservando parâmetros, aliases e formatos de chamada. Busca e descoberta
inicial ignoram `.venv`; acesso explícito continua disponível por `read`, `list`
e `shell`. A descrição de leitura orienta calcular agregados localmente.
Escrita e patch acrescentam diagnóstico de sintaxe quando introduzem JSON inválido
em arquivos `.json` de até 1 MiB. O aviso acompanha a escrita bem-sucedida, não
desfaz alterações nem comprova conclusão da tarefa. Templates previamente
inválidos e outros formatos ficam fora dessa verificação. [Evidência e limites](release/README.md).

**Checkout — seleção textual e confirmação de cópia, 14/09/2026:** o arrasto
fica no conteúdo da conversa ou do inspetor, sem alcançar os controles e sem
pintar espaços vazios à direita. Clique direito copia a seleção atual; `Copiado`
aparece discretamente após sucesso do clipboard. [Validação e limites](docs/AUDIT-SLIM-TUI-TRACKER.md).
Deploy local concluído às 21:05; [identidade do executável](release/README.md).

**Checkout — polimento adicional da TUI, 14/09/2026:** navegação por mouse no
inspetor, reutilização das métricas de scroll, agendamento sem frame duplicado
e indicador Thinking separado das linhas estáveis. Menus têm seleção com fundo
distinto; espera pelo provider e raciocínio são estados diferentes, sem repetir
Thinking na barra quando seu cabeçalho está visível. Deploy local concluído em
14/09 às 20:29; veja a [validação](docs/AUDIT-SLIM-TUI-TRACKER.md)
e a [identidade do executável instalado](release/README.md).

**Checkout — 14/09/2026, revisão da TUI:** fundo preto com superfícies neutras,
editor com quebra visual sem alterar o prompt, rodapé compacto que preserva
cancelamento e novas mensagens, navegação visível em menus e rolagem própria
dos inspetores. A busca prioriza a consulta; o TODO não duplica o indicador
animado de atividade. Confirmações recebem realce breve, respeitando movimento
reduzido. Veja o [contrato visual](docs/DESIGN-SLIM-TUI.md)
e o [registro de validação](docs/AUDIT-SLIM-TUI-TRACKER.md).
Esta revisão foi implantada localmente em 14/09/2026 às 19:41; identidade e
smoke do executável estão no [registro de deploy](release/README.md).

**Checkout — 12/09/2026, agrupamento da TUI:** trechos do assistente entre
mensagens do usuário compartilham um único cabeçalho `Slim`. Textos e atividades
mantêm sua ordem, com indicação de interrupção/falha no grupo. Rascunhos e
mensagens enfileiradas não iniciam outra resposta visual. Veja o
[contrato visual](docs/DESIGN-SLIM-TUI.md) e o
[registro de validação](docs/AUDIT-SLIM-TUI-TRACKER.md).
Esta mudança está apenas no checkout, sem novo deploy.

**Checkout — 12/09/2026:** revisão dos contratos de ferramentas da auditoria
unificada, descrita em [Admissão e resultados nativos](#admissão-e-resultados-nativos).
Esta mudança está apenas no checkout; a identificação do executável instalado
abaixo pertence ao deploy anterior.

**Checkout — 10/09/2026:** TUI com coluna de leitura de até 100 células, paleta
quente, footer responsivo, pulso discreto de atividade e respostas identificadas
por `Slim`, com faixa do usuário mais suave. Pensamento tem pulso contextual
e indicação de detalhes recolhidos/expandidos. Veja o
[contrato visual](docs/DESIGN-SLIM-TUI.md) e a
[validação atual e seus limites](docs/AUDIT-SLIM-TUI-TRACKER.md).
Esta revisão também está no `Slim.exe` do PATH, atualizado em 10/09/2026 às
16:39:46. [Identidade do build e smoke do deploy](release/README.md).

> **⚠️ Agentes de código:** leiam [RULES.md](RULES.md) e [AGENTS.md](AGENTS.md)
> **antes de qualquer tarefa**. Regra Zero: toda mudança de código termina com
> `.\refresh-slim.ps1` imprimindo `OK:` — o `slim` do PATH é uma cópia estática
> e não acompanha o código sozinho. Proibido afirmar "feito/testado" sem
> evidência executada na sessão.

Harness de coding agent em Rust para Windows x64/MSVC (`0.1.0`). **Status de
implementação: checkpoint de integração parcial, não v1 concluída.** Headless e
TUI fullscreen usam provider, agent loop e tools reais; a TUI recebe streaming,
usage e cancelamento por channels tipados, sem executar domínio.
