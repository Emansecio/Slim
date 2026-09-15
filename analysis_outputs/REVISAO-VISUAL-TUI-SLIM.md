# Revisão visual da TUI do Slim

Data: 2026-09-04. Workspace: `D:\Slim`. Escopo: evolução da interface existente,
sem modificar o harness ou os protocolos dos providers.

## Resultado e direção

Implementados ajustes pequenos de hierarquia, contraste, densidade e clareza do
estado. Mantidos fundo near-black, verde Slim, azul dos controles, conversa
fluida, faixa do usuário, composer contornado e expansão das ferramentas.

Direção: **conversa em primeiro plano; metadados discretos mas legíveis;
atividade compreensível sem sinalizar alerta o tempo todo; detalhes claros
quando solicitados**. Não houve troca de tema, novo painel, configuração,
dependência ou animação.

**Limite da avaliação:** houve interação com o executável real por PTY, com
saída ANSI e fixture HTTP localhost. Não houve captura de pixels de um terminal
físico. Portanto, esta entrega não afirma validação visual completa, conforto
subjetivo no monitor, ausência de flicker ou aprovação da matriz física Windows.

## Base, referências e preservação

- Lidos `AGENTS.md`, `RULES.md`, contrato visual e relatórios
  `analysis_outputs/HARNESS-SLIM-ETAPA-{1,2,3,4,5}.md`. As etapas foram tratadas
  como histórico, preservando streaming, uso observado/estimado, causalidade,
  cancelamento, contexto, budgets e configuração efetiva.
- O checkout inicial estava modificado: OAuth/xAI, providers, runtime, tools,
  documentos e migração de 17 arquivos de testes TUI para `tests/integration/`.
  Nenhum reset, restore, stash, checkout, commit ou descarte foi realizado.
- A comparação de autoria usou hashes dos arquivos e o conteúdo inicial em
  memória. Alterações concorrentes de documentação sobre otimização de
  testes/build foram preservadas, inclusive seus gates históricos.
- O Pi foi consultado no footer atual de
  `C:\PiTest\packages\coding-agent\src\modes\interactive\components\footer.ts`:
  separação entre identidade, métricas e informação contextual. Essa consulta
  orientou simplicidade operacional; não foi uma avaliação visual do Pi nem
  transplante de componentes, animações ou arquitetura.
- Skills usadas como orientação: Impeccable/polish e brainstorming. A direção
  foi apresentada antes da edição; a autorização explícita do pedido cobriu
  implementação e revisão do contrato.

## Executável observado e método

O `Get-Command slim` inicial resolveu `C:\Users\User\bin\Slim.exe`, cópia
release de 20:56:45. Havia debug mais recente; foi recompilado antes de tomar a
base comparável. Baseline observada: `D:\Slim\target\debug\slim.exe`, build
21:22:47, 31.012.864 bytes, SHA-256
`F7D87A016CCCF60353436DDEFD0433FF3D84FF5A8D8A846CF61BE4FF3B83960E`.

Os ajustes visuais foram observados depois no release instalado pelo script.
Debug/release não foram comparados como benchmark de velocidade.

Uma tentativa de abrir `conhost` para captura física foi rejeitada pela revisão
automática de aprovação com “blocked by policy”, sem razão mais específica.
A observação continuou pelo PTY disponível; não houve tentativa de contornar
a rejeição ou fabricar screenshots.

`--fake` não seleciona uma execução fake na TUI. A primeira abertura com essa
flag foi encerrada sem enviar prompt. Os exemplos seguintes usaram
explicitamente `--provider openai`, modelo de fixture, credencial fictícia
restrita ao processo e `--endpoint http://127.0.0.1:18197/v1/chat/completions`.
O servidor temporário só emitiu reasoning/texto fixos e duas chamadas `read`:
`README.md` e um caminho inexistente. Nenhuma chamada paga, escrita por tool,
autenticação, troca de modelo persistida ou inferência comercial foi feita.

A primeira versão da fixture usou paths absolutos, corretamente recusados pela
tool. Foi corrigida para paths relativos antes de verificar sucesso + falha.
A resposta da fixture é texto ilustrativo fixo, não uma avaliação produzida por
modelo. O README já contém trechos com mojibake; isso apareceu no output antes
e depois, sem ser atribuído à nova paleta ou corrigido oportunisticamente.

## Diagnóstico do conjunto e decisões

| Área | Evidência e decisão |
|---|---|
| Conversa, reasoning e resultados | User band, labels, reasoning recolhido e headings já distinguem os papéis. Mantidos sem cards ou limite artificial de largura. |
| Sessão e composer | A identidade da sessão era azul/negrito, competindo com controles e resposta. Passou a muted. Nome de modelo longo desaparecia por inteiro no fallback do label; agora é abreviado com elipse, preservando esforço e modo. |
| Paleta e legibilidade | Muted `#747B84` tinha contraste calculado de 4,46:1 sobre `#0D1014`; `#828A94` chega a 5,46:1, e 4,69:1 sobre a faixa do usuário. É cálculo dos tokens sRGB, não medição do monitor. |
| Atividade | O início mostrava budgets zerados e `Ctrl+C` em duas linhas. Agora fase/spinner são neutros, contadores zerados somem e o atalho permanece no footer. Contadores não zerados só entram inteiros quando cabem; ao atingir 80% do limite recebem warning. Avisos de orçamento do harness permanecem intactos. |
| Tools e outputs | Sucessos compactos e falhas com razão curta já funcionavam. Mantidos. Output expandido era tão apagado quanto metadata; usa agora `secondary_text`, com contraste calculado de 8,71:1 sobre o transcript. Sem alterar paginação, fold, retenção ou número de linhas. |
| ANSI16 | A conversão escolhia o maior canal e transformava tanto âmbar quanto erro em vermelho. Agora amarelo e vermelho claro são distintos; azul/ciano usam variantes claras. A paleta configurada pelo terminal ainda determina o resultado físico. |
| Modo inicial | A TUI aberta com `--read-only` mostrava `Auto`. `TuiStartup.mode` estava correto, mas o host só publicava `ModeChanged` depois de interações. Agora publica o evento existente logo após `WorkspaceChanged`, sem alterar permissões ou o loop. |
| Menus, ajuda e seletores | Mantidos filtro, seleção textual `>`, agrupamentos, Escape e inspectors. O contraste maior beneficia essas superfícies sem acrescentar bordas ou controles. Seleção comercial de modelo/login não foi executada. |
| Espaço, navegação e histórico | Mantidos composer adaptativo, reflow, scrollbar condicional, âncora por bloco, PageDown e End. Nenhuma cópia adicional de histórico, cache ou scheduler foi introduzida. |
| Uso e contexto | Totais, contexto e tok/s preservam os mecanismos existentes, inclusive `~` e knownness. Esforço exibido continua sendo estado da UI/configuração; o default High não prova que todo provider o recebeu, limite já documentado na etapa 5. |

## Antes/depois

Não foram capturadas comparações de pixels. A tabela registra composições
confirmadas na saída do PTY e no renderer; **não é um screenshot**.

| Situação equivalente | Antes | Depois |
|---|---|---|
| Thinking, 80×24, início da fixture | `Thinking · turn 1/128 · reads 0/96 · edits 0/32` e `Ctrl+C stop` na ActivityRail | `Thinking · turn 1/128`; cancelamento apenas no footer |
| Header da conversa, 80×24 | `SLIM · D:\Slim` em accent/negrito | Mesmo conteúdo em muted |
| Output aberto com Up/Enter, 80×24 | Texto `#747B84` | Texto `#A9B0B8`; metadata `#828A94` |
| Modelo longo | Fallback removia o modelo para conservar `(high) · Auto` | Em 60×16: `model-with-a-very-long-provider-qualifi… (high) · Auto` |
| Início com `--read-only` | Composer `Auto` | Projeção inicial de `Read-only`, pelo evento existente |
| Aviso/erro em ANSI16 | Ambos `Red` | `Yellow` / `LightRed`, cobertos pela matriz existente |

As comparações de composição usam a mesma fixture local e largura de 80×24
quando indicado. O histórico acumulado e os tempos da fixture variam entre
execuções; não há comparação de latência nem alegação de frames idênticos.

## Estados e interações realmente verificados

| Meio | Tamanho/capacidade | Verificado |
|---|---|---|
| PTY, baseline debug e release visual | 80×24, truecolor | Welcome; envio; reasoning parcial; tools read com sucesso/falha; resposta Markdown e diff; retorno ao idle; contexto estimado e posterior usage de fixture. |
| PTY, baseline e release visual | 80×24 | Up/Enter expandem falha e leitura extensa; PageDown; End; Ctrl+P, filtro `/activity`, Enter e inspector; Escape; Ctrl+C encerra a TUI. |
| PTY, release visual | 60×16, truecolor + reduced motion | Nome longo com elipse; espera local; glyph estático; elapsed semântico; Ctrl+C durante espera; `run cancelled`; retorno ao composer e saída. |
| PTY, release visual | 40×12, ANSI16 + reduced motion | Welcome/composer; modelo abreviado; fluxo local, Markdown/diff e footer compacto com contexto/usage. |
| PTY, release visual | 32×10, NO_COLOR + reduced motion | Layout de emergência, composer sem box, comandos/filtro e saída; sem SGR cromático da aplicação. Não houve fluxo completo de tools nessa dimensão. |
| PTY, release final 21:41:28 | 60×16, truecolor + reduced motion | Boot com `--read-only`: composer mostra `visual-local (high) · Read-only`, corrigindo a inconsistência observada. |
| TestBackend e testes existentes | Larguras 40, 80, 99, 100, 139, 140, 200 × alturas 8, 12, 24, 40; emergência 39×7 | Invariantes de região, composer/status, Unicode, overlays, scroll/fold, fault/property, cores e motion. Isto não comprova aparência em terminal físico. |
| Teste de label | Larguras úteis 38, 58, 78, 118 | Modelo longo com caracteres largos, elipse, esforço/modo, limite de células e estado sem autenticação. |

Os tamanhos PTY foram configurados por `mode.com con cols=... lines=...`, com
sequências de resize observadas. Não foi realizado arraste/redimensionamento
contínuo de uma janela física. Mouse, clipboard real, seleção nativa, IME,
fontes alternativas e sessão comercial longa continuam sem validação manual.

## Implementação, custo e cobertura

Fontes de produção alterados: `crates/slim-tui/src/{runtime,theme}.rs` e uma
publicação de evento em `crates/slim-cli/src/tui.rs`. O restante é contrato,
índices/status e testes diretamente relacionados. Core, adapters, protocolo,
caches, limites, scheduling e ferramentas não foram modificados por esta revisão.

Foi acrescentada uma função de teste para o modelo longo. Os testes existentes
de cores, atividade, resumo de tool e boot da bridge foram ampliados/atualizados.
A verificação da tool passou a delimitar a região do transcript, sem depender
da presença do texto `Ctrl+C` para excluir a ActivityRail. Não há snapshots novos,
framework visual, dependência ou timer.

O trabalho por frame continua limitado aos mesmos dados: no máximo três
contadores locais na ActivityRail; estilos/tokens e truncamento por células já
usados pelo produto. O novo evento de modo ocorre uma vez no boot. Nenhuma
varredura/cópia nova do histórico foi acrescentada.

Benchmark existente, `cargo bench -p slim-tui --bench long_session`, exit 0:
3.200 blocos, 4.968.694 bytes; `input_to_frame_p95_ms=1.934`,
`scroll_locate_p95_ms=0.702`, abaixo do gate de 16 ms. Medição em TestBackend,
sem baseline de desempenho nesta revisão e sem promessa de redução de CPU,
latência comercial ou flicker.

## Validação técnica e limites

- `cargo test -p slim-tui`: passou após atualizar expectativas das cores
  deliberadamente revisadas e a delimitação do transcript no teste de tool.
- Boot ReadOnly: teste existente
  `new_tui_emits_workspace_before_other_startup_state` falhou antes com
  `AuthStateChanged` em vez de `ModeChanged { ReadOnly }`; passou após a correção.
  Endpoint não utilizado `127.0.0.1:9`, sem prompt ou rede.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0 após a
  correção do modo.
- `cargo fmt --all -- --check`: há drift preexistente em OAuth, LSP,
  reducer/inspector e testes fora dos trechos alterados. Não foi aplicada
  formatação global. Os seis arquivos de renderer/tema/testes visuais passaram
  no rustfmt individual. Trechos adicionados à bridge foram formatados sem
  reformatar seu WIP preexistente.
- A primeira tentativa do gate integral falhou por `Acesso negado` ao substituir
  o debug ainda usado na observação. A sessão de observação foi encerrada
  normalmente e o gate foi repetido; não houve exclusão de executáveis,
  limpeza de target ou encerramento de processos alheios.

## Gate final e deploy

`.\refresh-slim.ps1 -Test` executou `cargo test --workspace` sem filtros:
**1106 passed / 0 failed / 1 ignored / 72 suítes / 0 compiler warnings**,
incluindo doc-tests. Números somados das linhas `test result: ok` de `delivery.log`
nesta sessão, após a correção do modo. Exit 0. O único ignored continua sendo
o teste físico ConPTY; a observação por PTY desta tarefa não o promove a aprovado.

Saída literal do deploy:

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 21:41:28)
```

`target/release/slim.exe` e `C:\Users\User\bin\Slim.exe`: 15.014.912 bytes,
SHA-256 idêntico
`407598446856B288086EB9B78CADC39B6F2495E5C37ED198909287B3CDBC6F6C`.
Binário instalado: `--version` → `slim 0.1.0`; `--headless --fake 'revisao visual final'`
→ `success`, ambos exit 0. Toolchain: rustc 1.98.0 (88d9e12ae 2026-08-18), Scoop/MSVC.

As capturas de fluxo/cores anteriores usaram o release visual de 21:32:54;
a única mudança posterior de produção foi publicar o modo inicial. O release
final teve esse estado conferido novamente por PTY. Não são screenshots físicos.

Logs e fixture auxiliares desta execução estão em
`C:\Users\User\AppData\Local\Temp\slim-visual-review-20260905`:
`tui-tests.log`, `mode-red.log`, `mode-green.log`, `clippy-final.log`,
`delivery.log`, `bench.log`, `fmt.log`, `fixture.py`. Não são infraestrutura
nova do projeto; a evidência principal está reproduzida neste relatório.

### Verificação de entrega — RULES.md §4

Revisão final: `git diff --check` global exit 0; seis links dos índices/status
para este relatório resolvidos em disco; nenhum hash/contador anterior nos
READMEs/Plano correntes. Comparação final de hashes identificou somente os oito
fontes/testes listados no escopo da revisão. Helpers e sessões de observação
foram encerrados, preservando os arquivos temporários de evidência.

- [x] Rodei o que afirmo ter rodado: comandos, resultados e `OK:` registrados acima.
- [x] Números citados contados nesta sessão; benchmark do renderer separado da experiência física.
- [x] Arquivos e símbolos citados lidos nesta sessão; autoria separada do diff total contra HEAD.
- [x] `cargo test --workspace` verde: 1106 passed, 0 failed, 1 ignored, 0 warnings.
- [x] `.\refresh-slim.ps1 -Test` executado, imprimiu `OK:`; hashes target/PATH iguais.
- [x] Contrato, relatório, índices e status atualizados; contagens/hashes anteriores preservados somente em registros históricos.
- [x] Incertezas declaradas: captura física bloqueada, limitações do PTY, fmt preexistente, fontes/IME/mouse/clipboard e providers comerciais não validados.

Não aplicáveis: alteração do harness/protocolos, publicação remota, commit/push,
regeneração do ZIP, novas dependências e chamadas pagas. O deploy é a cópia local
do executável exigida pelo projeto.

## Ideias descartadas

- Redesenhar welcome, copiar a aparência do Pi ou remover todos os indicadores.
- Cards por mensagem, novas bordas, ícones, gradientes, barras sem progresso
  conhecido, animações ou preferências adicionais.
- Esconder erros no agregado de sucesso, recolher falhas sem razão ou reduzir
  o conteúdo que a pessoa explicitamente expandiu.
- Impor uma coluna estreita à conversa, reescrever scroll/cache ou acrescentar
  screenshots como testes de aparência física.
- Migrar providers, corrigir o `--fake` da TUI ou reformular a representação de
  esforço como efeito colateral. A fixture local contornou a necessidade de
  chamada comercial, sem alterar esses contratos.

O contrato visual e os índices apontam para esta evidência. A matriz física
permanece aberta; os resultados aqui não a declaram concluída.
