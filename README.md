# Slim

**Checkout — compactação por poda Jev, 18/09/2026:** a compactação passa a ter
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
`jev-latest`) e a rota de avaliação do Vercel AI Gateway
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

**Checkout — backend Vercel do Jev, 19/09/2026:** sem `SLIM_JEV_BACKEND`, a
própria chave decide o endpoint: `vck_` é o prefixo que a Vercel emite, então
uma chave do gateway basta e uma `TYPESAFE_API_KEY` comum continua na TypeSafe
(uma chave `vck_` guardada em `TYPESAFE_API_KEY` também chega ao gateway). O
contrato do gateway difere do TypeSafe em dois pontos, tratados no mesmo
módulo: o modelo viaja no header `ai-model-id` e o primitivo booleano chama-se
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

**Checkout — remoção do modo Jev, 18/09/2026:** o modo Jev (roteamento de
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
estado do checkout; [validação](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md)
registra os gates.

**Checkout — feedback e leitura da TUI, 15/09/2026:** atividade acompanha as
chamadas paralelas por identidade; preparação, admissão, execução, retry e
cancelamento têm estados distintos. A fila fica pausada após interrupção
solicitada e oferece `/queue status|pause|resume|edit N|remove N`. O pensamento
usa uma prévia incremental de duas linhas; seleção, aprovações, TODO,
notificações e encerramento têm comportamento explícito. Coluna central de
100 células (workspace de até 144 com inspetor), interface em português e
ênfases breves respeitam movimento reduzido. [Contrato](Documentações%20-%20Projeto/DESIGN-SLIM-TUI.md#12-direção-visual-revisada),
[validação](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md) e
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
aparece discretamente após sucesso do clipboard. [Validação e limites](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Deploy local concluído às 21:05; [identidade do executável](release/README.md).

**Checkout — polimento adicional da TUI, 14/09/2026:** navegação por mouse no
inspetor, reutilização das métricas de scroll, agendamento sem frame duplicado
e indicador Thinking separado das linhas estáveis. Menus têm seleção com fundo
distinto; espera pelo provider e raciocínio são estados diferentes, sem repetir
Thinking na barra quando seu cabeçalho está visível. Deploy local concluído em
14/09 às 20:29; veja a [validação](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md)
e a [identidade do executável instalado](release/README.md).

**Checkout — 14/09/2026, revisão da TUI:** fundo preto com superfícies neutras,
editor com quebra visual sem alterar o prompt, rodapé compacto que preserva
cancelamento e novas mensagens, navegação visível em menus e rolagem própria
dos inspetores. A busca prioriza a consulta; o TODO não duplica o indicador
animado de atividade. Confirmações recebem realce breve, respeitando movimento
reduzido. Veja o [contrato visual](Documentações%20-%20Projeto/DESIGN-SLIM-TUI.md)
e o [registro de validação](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Esta revisão foi implantada localmente em 14/09/2026 às 19:41; identidade e
smoke do executável estão no [registro de deploy](release/README.md).

**Checkout — 12/09/2026, agrupamento da TUI:** trechos do assistente entre
mensagens do usuário compartilham um único cabeçalho `Slim`. Textos e atividades
mantêm sua ordem, com indicação de interrupção/falha no grupo. Rascunhos e
mensagens enfileiradas não iniciam outra resposta visual. Veja o
[contrato visual](Documentações%20-%20Projeto/DESIGN-SLIM-TUI.md) e o
[registro de validação](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Esta mudança está apenas no checkout, sem novo deploy.

**Checkout — 12/09/2026:** revisão dos contratos de ferramentas da auditoria
unificada, descrita em [Admissão e resultados nativos](#admissão-e-resultados-nativos).
Esta mudança está apenas no checkout; a identificação do executável instalado
abaixo pertence ao deploy anterior.

**Checkout — 10/09/2026:** TUI com coluna de leitura de até 100 células, paleta
quente, footer responsivo, pulso discreto de atividade e respostas identificadas
por `Slim`, com faixa do usuário mais suave. Pensamento tem pulso contextual
e indicação de detalhes recolhidos/expandidos. Veja o
[contrato visual](Documentações%20-%20Projeto/DESIGN-SLIM-TUI.md) e a
[validação atual e seus limites](Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
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
qualquer tarefa que altere código** — ver [AGENTS.md](AGENTS.md). O script
também corrige o `RUSTC` da sessão (as variáveis de usuário `RUSTC`/`CARGO`
apontam para um caminho inexistente; detalhes no tracker TUI §8.3).

## Configuração (`slim.toml`)

O Slim lê defaults de dois arquivos TOML, mesclados nessa precedência:

**flag CLI > variável de ambiente > projeto > global > default interno**

| Camada | Caminho |
|---|---|
| Projeto | `./slim.toml` (diretório de trabalho) |
| Global | `%APPDATA%\slim\slim.toml` no Windows |

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
recuperações contam no limite de turnos; se não bastarem, o Slim informa a
interrupção e orienta ajustar o orçamento ou reduzir o próximo passo.
Se o provider rejeitar o aumento por erro no parâmetro de saída, a recuperação
restante volta ao orçamento original, com aviso explícito.

- Arquivo ausente é normal; **arquivo presente e inválido aborta** com erro
  nomeando o caminho — sem silenciar configuração quebrada.
- As camadas só entram em jogo quando o modo provider está ativo (flag ou env);
  config sozinha não ativa rede.

Tuning de velocidade (env vence o TOML): SLIM_MAX_TURNS, SLIM_MAX_MUTATING_TOOL_CALLS,
SLIM_MAX_READ_TOOL_CALLS, SLIM_MAX_OUTPUT_TOKENS, SLIM_TIMEOUT_SECS, SLIM_MAX_RESULT_BYTES.
Defaults: turns 128, mutating 32/turno, read 96/turno, output pelo catálogo do modelo (4096 se desconhecido; reserva limitada à metade da janela), timeout 120s, result 16 KiB (read completo até 64 KiB).
Reduzir timeout_secs da fail-fast em rede lenta; reduzir max_turns/max_*_tool_calls encurta turnos longos.

## Admissão e resultados nativos

A ferramenta `skill` exige `list` booleano e `script` string quando fornecidos;
tipos inválidos, inclusive `null`, são rejeitados antes do despacho. Campos
omitidos mantêm os padrões existentes. Essa validação não acrescenta chamadas
ao modelo.

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
  `timeout_ms` aceita inteiros entre 1 e 120.000; fora da faixa a chamada falha
  na admissão, antes de executar.
- `read.lines` é alias de `max_lines`, com a mesma faixa de 1 a 4.096.
  Valores iguais nas duas chaves são aceitos; conflito, tipo inválido e campos
  desconhecidos são rejeitados. Exemplo: `{"path":"README.md","offset":10,"lines":12}`.
  Sem limite explícito, a primeira página pode ler até 4.096 linhas dentro do
  orçamento de bytes; páginas posteriores usam 200. Leitura parcial continua
  sem fornecer o recibo completo necessário para overwrite sem `expected`.
- As formas nativas de patch `edits` e `expected`/`replacement` continuam aceitas,
  de modo exclusivo. Campos desconhecidos também são rejeitados em cada edit.
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
  de saída, timeout, cancelamento, bytes observados e descartados. Código zero
  descreve o processo; não certifica todas as operações do script nem a tarefa.
  Ausência de diagnóstico estruturado continua sem classificação automática.
- A ferramenta shell retém até 8 KiB por stream para a prévia, mantendo início,
  fim e contagem de descarte, com drenagem, progresso e cancelamento. APIs públicas
  de captura bruta preservam seus orçamentos anteriores. O artefato desse caminho
  contém a saída apresentada, não recupera bytes já descartados na captura.
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
No wire Chat Completions, snapshots cumulativos de `usage` do OpenCode Go são
consolidados em um único total conservador; providers irmãos continuam com
validação terminal estrita.
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

Deploy desse ciclo: `.\refresh-slim.ps1` com `OK:` (build release, cópia para o
PATH e `slim --version` = `slim 0.1.0` com exit `0`). O estado de validação
**vigente** está no checkout do topo desta página e no
[registro de deploy](release/README.md).

Auditorias persistentes: [WORKFLOW-LOOP-BUGS-SLIM.md](analysis_outputs/WORKFLOW-LOOP-BUGS-SLIM.md)
e [TEST-SUITE-OPTIMIZATION.md](analysis_outputs/TEST-SUITE-OPTIMIZATION.md)
(índice completo em [analysis_outputs/README.md](analysis_outputs/README.md)).

## Status atual de integração

| Área | Estado atual |
|---|---|
| Headless/provider/tools/auth/compaction/usage/artifacts/anti-loop | `Slim --headless` usa caminhos integrados e exercitados offline; OpenAI-compatible/Anthropic SSE, read/list/search/write/patch/shell e filtragem de capabilities funcionam. |
| TUI | `Slim` abre a interface normal mesmo deslogado. `/login` abre seletor OAuth nativo para Claude Pro/Max ou ChatGPT Plus/Pro; Anthropic Messages e Codex Responses usam o mesmo agent loop/tools Slim. Reducer único normativo (`Action → reduce → Effect`), scrollback virtualizado e navegável (pin/live-edge/unseen), paleta estratificada §21.3, ActivityRail por fase/tempo, SessionRail conversacional adaptativa com contexto único e footer Grok-style responsivo (box alinhado + atalhos/metrics), tool blocks tipados agregados, lanes bounded com coalescer no runtime real, command palette Ctrl+P, markdown-light, motion básico com reduced motion e welcome estática com nome, conexão e próxima ação. Fixtures localhost provam OAuth/callback/store, streaming, tool round-trip e cancelamento. PTY E2E físico pendente de console real (teste `#[ignore]`). |
| Cache/HTTP | Replay local de respostas (`ProviderCache`) desligado no produto; transporte HTTP compartilhado reaproveita conexões, enquanto adapters e autenticação continuam isolados por request. |
| Sessões | Writer/recovery/branch e `--session` escrevem JSONL novo; `--resume`/recovery explícitos existem headless e TUI; UX geral de seleção/fork continua limitada. |
| Skills, MCP, subagentes | Tool `skill` lazy (`list` / `name`); pasta `%USERPROFILE%/.slim/skills` (cwd `.slim/skills` vence). Sem injeção de catálogo. MCP stdio/HTTP lazy via meta-tool `mcp` + overlay `/mcp` (ver seção MCP). Sem child. |
| Todo/Plan/Goal | Tool `todo` no loop Auto; `TodoChanged` abre o dock TUI. Plan/Goal sem UI; Plan headless continua abortando (N4). |
| Release | Determinístico e hash-valid; empacota este checkpoint parcial, não uma v1 completa. |

O restante desta página descreve contratos realmente integrados. Para o plano de
integração pendente, consulte o [índice canônico](Documenta%C3%A7%C3%B5es%20-%20Projeto/README.md)
e o [plano](Documenta%C3%A7%C3%B5es%20-%20Projeto/PLANO-IMPLEMENTACAO.md). A fila
curta do ciclo atual (agente completo, harness leve) está em
[próximas etapas](Documenta%C3%A7%C3%B5es%20-%20Projeto/PROXIMAS-ETAPAS-AGENTE.md).

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

O runtime aplica budgets separados read-only vs mutating por run (`max_read_tool_calls` default 96, `max_mutating_tool_calls` default 32; env `SLIM_MAX_READ_TOOL_CALLS` / `SLIM_MAX_MUTATING_TOOL_CALLS` e chaves `slim.toml`); esgotamento → `tool_limit` (exit 22). O teto de turns por run é 128 (`SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024); esgotamento → `turn_limit` (exit 12) com mensagem `Turn limit reached (N/N)`. A tool `search` respeita `.gitignore`, ignora árvores de build, cap default 200 hits e paginação `offset`/`max_hits`. Emite `ToolStarted`, mantém pares assistant/tool e o prompt raiz através da
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

`--image PATH` aceita repetidamente PNG, JPEG/JPG, GIF e WebP; cada entrada deve
ser arquivo regular não-symlink, não vazio e ter no máximo 20 MiB. O conteúdo é
enviado como base64 estrito, sem I/O remoto. `SLIM_CONTEXT_WINDOW_TOKENS` e
`SLIM_MAX_OUTPUT_TOKENS` aceitam inteiros positivos e controlam janela/reserva
de contexto e, quando suportado pelo wire, o teto de saída do provider (padrão:
4096 tokens). No contrato Codex subscription, o valor permanece reserva local e
`max_output_tokens` não é serializado. A apresentação dos resultados usa um
orçamento agregado calculado com o request serializado, a estimativa conservadora
do preflight, os schemas, o histórico e a reserva de saída. Leituras completas
de até 64 KiB continuam possíveis quando cabem. Páginas menores preservam
registros inteiros e calculam a continuação pelo conteúdo entregue; registros
grandes demais recebem um aviso explícito. Resultados completos são mantidos
separadamente, com artefato quando o armazenamento está configurado.
O diário mantém o resultado bruto e a projeção vinculada à sua entrada; resume
e branches aplicam essa projeção antes de verificar checkpoints. A projeção
segue a gravação de fatos do diário e é sincronizada pelo próximo append durável.

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
[Documentações - Projeto/README.md](Documenta%C3%A7%C3%B5es%20-%20Projeto/README.md).

As decisões normativas estão em
[DECISOES-GRILL-PRE-IMPLEMENTACAO.md](Documenta%C3%A7%C3%B5es%20-%20Projeto/DECISOES-GRILL-PRE-IMPLEMENTACAO.md)
e o plano/evidência final em
[PLANO-IMPLEMENTACAO.md](Documenta%C3%A7%C3%B5es%20-%20Projeto/PLANO-IMPLEMENTACAO.md) e
[POC-RESULTS.md](POC-RESULTS.md).

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
- `Ctrl+V`/`Shift+Insert` e o paste do botão direito: com imagem no clipboard,
  o composer recebe um chip `image · clipboard-N.png` (PNG temporário anexado
  ao próximo prompt); sem imagem, cola texto como antes. `/image PATH` continua
  anexando arquivos locais;
- `Esc`: cancela seletor/login em andamento.

OAuth usa PKCE S256, callback restrito a loopback, validação de `state`, refresh
e browser via `ShellExecuteW` sem shell. Codex usa SSE Responses; WebSocket fica
fora deste checkpoint. Testes são localhost/offline: nenhum login real foi
executado, e política de limites/cobrança pertence aos providers.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).

This license applies to the original Slim code. Third-party dependencies and
materials retain their respective licenses and rights.
