# Slim — avaliação agentic de code_intel

## Resultado

**10/10 tarefas corretas; nenhuma chamada `code_intel`. Ganho de exploração ou
rodadas pelas capacidades semânticas recentes não demonstrado nesta amostra.**
`search.context_lines` foi escolhido espontaneamente em **15/17 searches**.
Sem comparação anterior reproduzível, não há afirmação de redução contra outra versão.
Nenhuma alteração em runtime, LSP, search, renderer, descrições ou system prompt.

Execuções reais com **openai-codex / gpt-5.6-luna / high / normal**, autorizadas
pelo usuário nesta sessão. Duas repetições por tarefa, mesma base por categoria,
processos sequenciais e workspaces novos em `%TEMP%`. Ordem A–E, depois A–E.
Não são respostas predeterminadas nem execuções do agente Pi no lugar do Slim.

## Baseline e condições

- HEAD `972b46e4e445de05b4021554b277710f3cfe9243`, **checkout sujo**. HEAD sozinho não identifica execução.
- [Manifesto](results/baseline.json): arquivos de crates/testes/Cargo com hashes;
  digest `1a7bd78580f9335f2e730520fc48ec2f3dff31fd2ae7c5c3e419b3857a81d4ea`.
- Build atual: `target/release/slim.exe`, SHA-256
  `8e3b6dd650e4656f00b60165d93210b2ec1e3698c905f3f7e36c532306bf27c0`.
  Não é executável instalado. Build release local, sem instalação/deploy.
- Python 3.12.10; Cargo/rustc/rustdoc 1.98.0; rust-analyzer disponível
  `1.98.0 (88d9e12a 2026-08-18)`. Nenhum servidor instalado.
- [Configuração isolada](config.toml): Auto, 20 turns, 40 calls totais, 32 read calls,
  12 mutating calls; timeout provider 180 s; LSP habilitado, request timeout 30 s.
  Config global não editada. Configuração extra de temperatura/seed não enviada;
  defaults da mesma implementação. Mesmo prompt de sistema nativo v1.7.
- Todas as 68 requisições registraram `system_bytes=1784`, `tool_schema_bytes=5904`;
  bytes iguais não provam conteúdo igual, mas fontes/binário/config ficaram congelados.
  [Probe local](results/schema-probe.json) confirmou payload Codex com modelo Luna,
  effort high e ferramentas `read/list/search/write/patch/shell/code_intel/todo/skill`.
  Esse probe não chama modelo comercial, usa credencial dummy e HTTP loopback.
  É evidência separada de disponibilidade do schema; não prova saúde do LSP nem
  captura retroativa dos payloads comerciais.
- Sem warmup prescrito ao agente; sem seed nem controle do cache remoto. Bases
  pequenas deliberadas; arquivos e oráculos congelados antes das chamadas.
  Ausência de `.git` é característica da fixture, não falha de runtime.

## Tarefas e correção

Objetivos completos em `prompt-A.txt` … `prompt-E.txt`. Nenhum menciona ferramenta,
LSP ou sequência. O agente recebe somente objetivo e fontes; oráculo fica fora do
workspace. Rust sem dependências externas; catálogo tem 160 funções antes do alvo.

| Caso | Objetivo | Critério objetivo |
|---|---|---|
| A — definição | Resolver alias de `checkout::submit`, alterar taxa efetiva 25→40 | Só `billing.rs` muda; assinatura `pub fn charge_total(cents: u32) -> u32`, retorno `u32`; testes de submit/invoice em 0, 1, 100, 99999; legado mantém +900 |
| B — referências | Renomear `billing::charge_total` para `invoice_total` | Definição + import em checkout + chamada direta em reports; alias `calculate` preservado; reference_files exatamente checkout/reports; strings, comentário e função legada intactos; API nova compila e retorna 125 para 100 |
| C — símbolos | Identificar método tardio `zeta_export_window` | Contêiner `ExportPolicy`; assinatura `pub fn zeta_export_window(&self, rows: usize, cap: usize) -> Option<usize>`; retorno/path corretos; nenhum fonte alterado |
| D — textual | Mensagem ativa `Payment pending`→`Payment queued` | Só valor `receipt.message` de runtime.json muda; comentário e docs preservados; resposta com arquivo/chave |
| E — semântico + textual | Resolver alias de delivery e migrar chave ativa | Implementação `billing::retry_key`; código + configuração usam `transport.max_attempts`, valor 3; chave legada e docs intactos; execução de `delivery::setting_name()` confirma nova chave |

Todas exigem `answer.json`. Validador confere transformação localizada esperada
(com tolerância de whitespace Rust, sem tolerância em strings/comentários), JSON
semanticamente exato, fontes protegidos byte a byte, arquivos novos não pertinentes
rejeitados e testes Rust independentes em **outra cópia temporária**. Oráculo é
restritivo por desenho: refatoração equivalente além da alteração pedida não passa.
Testes adicionais do agente são permitidos em `tests/`.

Autoteste do oráculo: **5 bases iniciais rejeitadas, 5 soluções canônicas aceitas,
5 corrupções de documentação protegida rejeitadas**. Não são resultados agentic.
[Detalhes](results/oracle-tests.json). Os 10 resultados reais passaram todos os
checks e testes externos; exit 0 do agente não foi usado sozinho como correção.

## Escolha de ferramentas

`R=read`, `S=search`, `H=shell`, `P=patch`, `W=write`, `L=list`.
Colchetes = chamadas emitidas na mesma resposta do modelo; **não garantem execução
simultânea**, especialmente mutações. Cada seta contém retorno ao modelo. Rodadas
contam modelo→ferramentas→modelo, não invocações nem `provider_turns − 1` inferidos.

| Execução | Sequência por batch | Rodadas | Calls | R/S/H/P+W/L | Resultado |
|---|---|---:|---:|---|---|
| A-1 | [R6] → [S+L] → [P+W] → [H+R3] → [H] → [H] → [H] | 7 | 17 | 9/1/4/2/1 | sucesso |
| B-1 | [S+R6] → [L+R4] → [P3] → [H+S2+R3] → [W] → [H3] → [H] | 7 | 26 | 13/3/5/4/1 | sucesso |
| C-1 | [R] → [W] → [R] | 3 | 3 | 2/0/0/1/0 | sucesso |
| D-1 | [R2+S] → [L] → [P] → [W] → [R3+S] | 5 | 10 | 5/2/0/2/1 | sucesso |
| E-1 | [R5+L] → [R+S+R2] → [P2+W] → [H+S+R] | 4 | 16 | 9/2/1/3/1 | sucesso |
| A-2 | [R6] → [S+L+R3] → [P] → [W] → [H] → [R2+H] | 6 | 17 | 11/1/2/2/1 | sucesso |
| B-2 | [S+R+L2] → [R8] → [P] → [P2] → [H+S2] → [R] → [W] → [R] | 8 | 21 | 11/3/1/4/2 | sucesso |
| C-2 | [S] → [R] → [W] | 3 | 3 | 1/1/0/1/0 | sucesso |
| D-2 | [R2+L] → [P] → [W] → [R3+H] | 4 | 9 | 5/0/1/2/1 | sucesso |
| E-2 | [R5] → [R+S2+L] → [R4] → [P] → [P] → [P] → [R] → [H+R4] → [L] → [W] → [R+H+S2] | 11 | 28 | 16/4/2/4/2 | sucesso |

Total: **150 calls, 58 rodadas, 68 provider turns**. Ferramentas: 82 read,
17 search, 16 shell, 15 patch, 10 write, 10 list; code_intel/todo/skill = 0.
Contadores do Slim: 150 executadas, nenhuma reutilizada/suprimida; nenhum resultado
faltante. Em A-1, seis reads iniciais custam **uma** rodada, não seis.
Argumentos e resultados completos: `results/<caso>-<repetição>/metrics.json`;
[resumo mecânico](results/summary.json), [sequência observável](results/observable-calls.txt).

## Capacidades recentes: utilização real

| Capacidade | Observação |
|---|---|
| definition preview | **Não exercitado**, 0 definition; nenhuma sequência definition→read a classificar |
| symbol annotations/priorização | **Não exercitadas**, 0 symbol; C-1 lê catálogo inteiro, C-2 busca com contexto e depois lê trecho |
| references/context | **Não exercitado**, 0 references; B usa search em 2/2, mas distingue referências de strings/homônimo corretamente em ambos |
| search.context_lines | **15/17 searches**, 8/10 tarefas contêm ao menos um search com contexto; adotado espontaneamente inclusive edição localizada |

Ausência de LSP não equivale automaticamente a escolha errada: aliases explícitos,
arquivos pequenos e caminhos sugestivos permitem resolução correta por leitura.
Consultas definition/references exigem linha/coluna, que o agente ainda precisaria
obter; adicionar consulta após read completo poderia aumentar calls nesta fixture.
Não houve benefício semântico observado nem prova de inferioridade dessas ferramentas.

## Redundância: revisão contextual, não heurística

[Revisão manual](manual-review.json) cobre **todas as 150 chamadas**, com índices,
razão e erros. Classes incluídas em cada metrics.json. Leituras para preservar
homônimos, preparar edição, resolver contexto amplo ou validar mutação não foram
eliminadas mecanicamente. `patch` faz sua própria leitura e correspondência exata;
`write` exige leitura completa de **arquivo existente**, não de arquivo novo.

| Padrão/caso | Frequência | Custo observável | Causa provável / ressalva |
|---|---|---|---|
| Catálogo não relacionado lido durante A/B/E | 4/10 execuções: A-2#10, B-1#9, B-2#10, E-2#12 | 4 calls, **25.698 bytes** de resultados persistidos | Exploração ampla escolhida pelo modelo. Busca já cobria alvo sem hits nesse arquivo; conteúdo só aritmética. B-2 lê 100 linhas e nem precisa continuar página |
| Outros reads/list informativos sem uso pertinente | Mais 10 calls; classe informativa total 14 | Total informativo **26.978 bytes**, incluindo catálogo acima | Config de recibo em tarefa de taxa; list tardio; funções sem ligação. Não são 14 rodadas: maioria veio em batch útil |
| search com trecho suficiente→read informativo | C-2#2, **1/2 tarefas C** | 1 call, 442 bytes; batch exclusivo | Search traz linhas 161–167 completas com assinatura/container. Sem mutação/truncamento/precondição no catálogo. Hipótese: preferência por confirmação; contrato já explica contexto. Caso isolado, não generalizar |
| Read atual→read atual sem mudança | E-2#20, 1 caso | 1 call, 163 bytes; batch misto | #17 já trouxe JSON completo; cargo não altera config. Recuperação #17 após erro continua legítima |
| Busca negativa repetida | D-1#10, 1 caso | 1 call, 89 bytes; batch misto | Search global anterior cobria src; apenas config/answer mudaram |
| Segundo cargo check sem mudança Rust | B-1#23, 1 caso | 1 call, 95 bytes; batch misto | Apenas answer foi criado desde check aprovado; validação repetida não necessária |
| Correção de vírgula inexistente | E-2#16, 1 caso | 1 patch rejeitado, 230 bytes; depois read de recuperação | Replacement anterior já continha vírgula; expected inventado. Erro correto protegeu arquivo; não mudar contrato por um caso |

Buscas pós-renomeação B foram classificadas como **reconsultas justificadas**:
estado mudou. E-2 envia duas buscas parcialmente sobrepostas no mesmo batch;
padrões/escopo não idênticos, registrado separadamente, fora da contagem estrita.
B-2 reads após search inspecionam usos/preservação; não presumir redundância porque
preview contém uma linha editável. Post-write read de answer verifica entrega e
não é confundido com read informativo do símbolo.

Nenhuma economia contrafactual de tempo/rodadas foi apresentada como medida.
C-2 tem batch só de read redundante: candidato a remover uma rodada, não uma
redução já validada por A/B. Catálogo em batches mistos aumenta bytes, mas sua
remoção isolada não necessariamente reduz rodadas.

## Erros e fallback

**8 chamadas com erro**, em 6/10 execuções, todas preservadas no conjunto:
- A-1: glob `route_fixture-*.rlib` incorreto; listagem via shell e glob `libroute_fixture-*.rlib` corrigem. Fallback legítimo.
- B-1: script usa `$null` como variável de resultado; corrigido para `$answer`.
- B-1, A-2, D-2: `git diff/status` fora de repositório; **15.619 bytes** somados de saída sem diff útil. Confundidor da fixture, não falha LSP.
- B-2: diretório tests ausente; depois read de answer ainda inexistente. Erro de read explica uso de write; criação seguinte funciona.
- E-2: zero matches no patch inventado; `file unchanged` + instrução de read. Recuperação correta, sem esconder falha.

Nenhuma ocorrência observada de rust-analyzer ausente, capability ausente,
starting, timeout, stale, símbolo não encontrado ou erro real **de code_intel**:
ferramenta não chamada. Não interpretar isso como saúde de LSP comprovada.

## Trabalho, bytes e tempo

| Execução | Resultados persistidos, bytes | Parede, s | Latência provider, s | Ferramentas, s |
|---|---:|---:|---:|---:|
| A-1 | 3.795 | 68,83 | 66,328 | 2,113 |
| B-1 | 20.371 | 53,58 | 51,775 | 1,740 |
| C-1 | 7.304 | 12,07 | 11,980 | 0,001 |
| D-1 | 1.917 | 28,37 | 28,240 | 0,043 |
| E-1 | 6.277 | 40,18 | 39,408 | 0,697 |
| A-2 | 12.635 | 57,69 | 56,347 | 1,297 |
| B-2 | 10.043 | 44,25 | 43,696 | 0,452 |
| C-2 | 1.107 | 15,36 | 15,272 | 0,004 |
| D-2 | 8.271 | 20,62 | 20,245 | 0,280 |
| E-2 | 13.522 | 95,32 | 94,044 | 1,139 |

Somados: parede **436,282 s**, provider **427,335 s**, ferramentas **7,766 s**;
85.242 bytes de resultados persistidos. Latência provider inclui transporte e
processamento local/remoto; não é espera pura nem latência atribuível ao runtime.
Tempo de ferramenta inclui shell externo: **não há separação confiável de Slim
interno versus subprocessos**. Não subtrair somas concorrentes para fabricar overhead.
Zero ms de ferramentas muito curtas é resolução do contador, não trabalho nulo.

Uso reportado pelo provider: **305.976 tokens de entrada** (incluindo cache),
**16.511 de saída**; total 322.487. Uso completo nas 10 execuções. Reasoning numérico
é subconjunto da saída, não somado novamente; conteúdo privado não capturado.
Custo faturado indisponível, não estimado. Sem converter bytes em tokens.

Resultados grandes semânticos não exercitados. Exemplo textual observado:
- C-1: read do catálogo retorna 7.108 bytes; próxima request registra
  history_bytes 2.797 e tool_result_bytes 7.360.
- C-2: search suficiente retorna 658 bytes; próxima request history_bytes 2.973,
  tool_result_bytes 758. Read redundante acrescenta 442 bytes persistidos;
  request seguinte registra history_bytes 5.790, tool_result_bytes 1.298.
- Ambos terminam com 3 rodadas/3 calls. C-2 tem menos bytes, mas parede maior:
  **15,36 s versus 12,07 s**. Não atribuir vantagem/latência ao runtime a partir disso.

`history_bytes`, `tool_schema_bytes`, `system_bytes` e `tool_result_bytes` são
componentes exportados, não bytes totais de payload HTTP. Resultados persistidos
não são iguais ao envelope reinserido no provider. Cada request e próximo contexto
por batch estão em metrics.json; alinhamento só feito quando requests=batched turns+final,
sem retry/erro/compactação (verdadeiro nas 10). Não há tokenização por resultado.

## Telemetria disponível e ausente

Reuso de `--jsonl` final e journal durável v2; nenhuma mudança no comportamento
para capturar ferramentas. `redact_durable_message` remove `responses_reasoning`
e `chat_reasoning`; sem trace de transporte, ReasoningDelta ou chain-of-thought.

| Campo | Registro |
|---|---|
| Ferramenta/argumentos/resultado/erro | Journal, join por tool_call_id; argumentos brutos e JSON em metrics.json; erros classificados manualmente |
| Início/fim/parede da execução | UTC + relógio monotônico externo, run.json |
| Início/fim por ferramenta | **null**, não exportados pelo journal; ordem/batch preservados |
| Tempo interno de ferramenta | Agregado por request/batch e total; duração individual null |
| Bytes de resultado | UTF-8 persistido; requests têm contadores próprios de reinserção |
| Reads | Paths/ranges pedidos e conteúdo retornado; bytes físicos de I/O **null**, não inferidos do comprimento da saída |
| Processos | 1 lançamento headless observado por execução; árvores/descendentes por ferramenta **null**; shell call não equivale a um processo |
| Requests LSP/stale/atual | **null** onde não exportados; zero chamadas code_intel não usado para inventar contadores internos |

## Decisão e próxima alteração

**Não implementar correção nesta rodada.** Padrão real: inspeção ampla e algumas
confirmações redundantes. Causa ainda não isolada entre fixture pequena, escolha
do modelo e instrução nativa de ler fontes pertinentes em conjunto. Schema já
menciona definition preview, natureza semântica de references, annotations em
symbol e contexto 1..3 em search. Não há padrão definition→read ou symbol→read
que justifique alterar esses resultados/renderer agora.

Próximo experimento útil: tarefa com resolução menos óbvia (trait/reexport e
consumidores distribuídos) mantendo Luna high, para testar escolha semântica sem
prescrever ferramenta. Se redundância de contexto suficiente se repetir, testar
**apenas descrição específica de search**, sem mexer em prompt global/runtime.
Nenhum roteamento search→definition, classificador auxiliar ou restrição de read.

## Reproduzir sem sobrescrever execuções

```text
python bench/agentic-code-intel/evaluate.py selftest
python -m unittest discover -s bench/agentic-code-intel -p test_harness.py -v
python bench/agentic-code-intel/analyze.py
```

`prepare` gera bases e manifestos uma vez, recusando bases existentes.
`run --authorized --case A --rep 1` só aceita autorização explícita e diretório de
resultado novo; as tentativas desta rodada já existem e não serão sobrescritas.
Não apagar logs/bases para repetir: uma próxima campanha deve ter outro diretório,
novo manifesto e autorização correspondente. O probe local não entra na amostra.

## Validação e preservação — RULES §4

- Leituras: AGENTS.md, RULES.md, avaliação anterior, CLI/headless/config,
  code_intel/schema, runtime/publicação de tools, discovery/manager,
  journal/usage, patch/write e prompt nativo. Referências estáticas distinguem-se
  das 10 execuções reais e do probe offline.
- `cargo --config 'build.rustc-wrapper=""' build --release -p slim-cli --locked`:
  **exit 0**, com rustc/rustdoc do mesmo toolchain, jobs=1, overrides só no processo.
  [Log](results/build.log). Sem alterações de produto; suíte completa/clippy/TUI
  **não aplicáveis**. Nenhuma contagem histórica reutilizada como check atual.
- Oráculo: 15 verificações descritas acima; execução real: 10/10 checks externos
  aprovados. [Unittest](results/harness-tests.log): **3 passaram**, incluindo
  batch paralelo≠duas rodadas, join de resultados fora de ordem, bytes UTF-8,
  resultados ausentes e prompts sem prescrição de ferramenta.
- Falhas de preparação preservadas: hash LF versus arquivo CRLF antes de lançar
  A-1, corrigido para hash dos bytes reais; probe inicial sem account_id dummy
  (exit 20, zero requests), seguido de probe local correto encerrado por HTTP 400
  intencional (exit 21, uma request). Utilitários de impressão sofreram cp1252;
  análise durável usa UTF-8 explícito. Nenhuma tentativa comercial descartada.
- `git diff --check`: **exit 0**. [Preservação](results/preservation.json):
  **18.329 arquivos preexistentes verificados por hash, zero alterados**.
  Snapshot não transacional; não abrange arquivos ignorados/bancos externos.
- Novos arquivos somente em `bench/agentic-code-intel/`; builds em target e tarefas
  descartáveis em TEMP. Resultados/oráculos não alteram projetos pessoais.
- Documentação desta avaliação criada; documentos históricos e WIP preservados.
  Deploy/refresh, commit/push/reset/clean e mudança do instalado **não aplicáveis
  e não executados**. Nenhuma janela aberta, minimizada, reorganizada ou focada.
- Limites: n=2 por objetivo, uma família pequena Rust, Windows/headless, paths
  sugestivos, Git ausente, sem baseline A/B, sem ferramenta semântica exercitada,
  cache/provider variáveis. Nenhum claim contra concorrentes nem economia causal.
