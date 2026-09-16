# Pi × Slim — uma tarefa real com GPT‑5.6 Luna

## Implementações nativas — 15/09/2026

O checkout reduz descrições das ferramentas sem mudar seus parâmetros, exclui
`.venv` da descoberta/busca e acrescenta diagnóstico de sintaxe de JSON à saída
de escrita e patch. Os avisos usam o conteúdo já em memória, após a operação
completa; não fazem rollback nem substituem o verificador da tarefa. Limitam-se
a `.json` de até 1 MiB, sem reinterpretar templates anteriormente inválidos.

Comparação antes/depois delimitada antes das chamadas: `merge_ranges`, `json_cli`,
`ledger_audit` e `config_migration`, duas rodadas, com a primeira versão alternada
por cenário e invertida na segunda rodada. Luna high/normal, fixtures idênticas,
execuções sequenciais e todos os resultados retidos. O baseline foi compilado do
checkout com seu trabalho preexistente, antes destas três mudanças; não é o
executável antigo instalado no PATH. A bateria mede o conjunto das mudanças,
sem isolar a contribuição de cada uma e sem nova comparação com Pi.

Logs e identidade do baseline ficam em `native-improvements-20260915/`;
campanhas individuais usam os sufixos `-native-before` e `-native-after`.
Os primeiros requests reais registraram **8.089 → 6.669 bytes de schemas
de ferramentas (-17,6%)**. Essa medida de bytes não equivale à variação de tokens
ou de tempo da tarefa completa.
A [evidência de testes e instalação](../../release/README.md) distingue essa
validação dos resultados históricos abaixo.

### Resultado do A/B nativo

**16/16 execuções aprovadas externamente, oito pares válidos.** Hashes das
fixtures, modelos e binários conferidos por par. Tokens totais:
**150.507 → 120.024 (-20,3%)**; tempo total **349,0s → 274,3s (-21,4%)**;
chamadas ao modelo **41 → 38**. Mediana das variações pareadas: -15,7% em tokens
e -23,8% em tempo. São duas rodadas por cenário: resultado exploratório, sem
afirmação de significância estatística ou superioridade sobre Pi/outros agentes.

| Cenário | Tokens, soma das duas rodadas | Tempo, soma das duas rodadas |
|---|---:|---:|
| merge_ranges | -12,7% | -27,0% |
| json_cli | -38,6% | -24,6% |
| ledger_audit | -10,8% | -27,2% |
| config_migration | -9,2% | -6,0% |

Os agregados não significam vitória em cada par: `ledger_audit` regrediu na
segunda rodada e `config_migration` na primeira. Falhas de ferramenta:
**2 → 3**; cache lido / tokens de entrada: **17,4% → 9,2%**. Tokens de entrada
sem cache caíram **12,8%** no total. Na primeira migração nova, duas escritas
foram rejeitadas por `expected` diferente dos bytes atuais, seguidas de recuperação
por patch. O rótulo heurístico `timeout` em `ab-results.json` para esses dois
eventos é incorreto: os outputs completos mostram `stale read`, sem timeout.

Na segunda migração nova, os dois patches produziram JSON inválido: o runtime
informou diretamente `production.json:11:7` e `development.json:18:7` nos resultados.
O modelo recebeu ambos os diagnósticos e a tarefa terminou aprovada pelo oráculo.
Isso valida a entrega live dos avisos; não isola quanto do ganho de tempo veio
deles. Na primeira auditoria de dados, ambas as versões ainda leram o CSV:
a orientação de usar agregados não garante que um modelo evite dados brutos.
A exclusão de `.venv` e o acesso explícito foram verificados por teste local.

Artefatos locais: `native-improvements-20260915/ab-protocol.json`, `ab-run.log`,
`ab-results.json` (por execução), `ab-summary.json` (agregado) e campanhas
`20260915-225405Z-native-before` até `20260915-230458Z-config_migration-native-after`.
As quatro builds antigas/temporárias foram removidas após a comparação; seus
caminhos, tamanhos e hashes permanecem em `removed-builds.json`. Logs, sessões,
workspaces de evidência e relatórios foram preservados.

## Bateria repetida — 15/09/2026

**32 pares Luna válidos:** Slim consumiu 14,1% mais tokens, levou 23,8% menos
tempo total e teve duas falhas de ferramenta contra onze do Pi. A mediana da
razão de tempo por par favoreceu o Slim em 9,3%; ambos passaram em todos os
verificadores. Oito pares adicionais com DeepSeek foram bloqueados pelo limite
mensal do OpenCode Go e não permitem comparação de desempenho.

[Diagnóstico, causas e próximos experimentos nativos](DIAGNOSTICO-SLIM-X-PI-2026-09-15.md),
[relatório Luna completo](RELATORIO-SLIM-X-PI-2026-09-15-CONTROLADO-LUNA.md) e
[registro do controle DeepSeek indisponível](RELATORIO-SLIM-X-PI-2026-09-15-CONTROLADO-DEEPSEEK.md).
O executável Slim permaneceu igual ao da amostra original; esta etapa corrigiu
o benchmark e investigou o comportamento, sem modificar ou instalar o produto.

## Medidor normalizado — 15/09/2026

O medidor distingue erro de JSON, falha de teste, erro de uso do Git e erro do
interpretador; `exit 1` sozinho nao identifica sintaxe de shell. Ferramentas Pi
sao associadas ao turno pelo identificador da chamada, sem deslocamento por horario.

Novas campanhas calculam hashes dos bytes efetivamente materializados antes da
execucao dos agentes. O diff registra modificados, criados e removidos, preservando
a deteccao de mudancas reais de LF/CRLF. Campanhas antigas com hash de texto fonte
podem reconstruir a materializacao Windows quando a fixture do registry confere
com o hash original do manifesto.

Os componentes de contexto usam conjuntos separados: sistema, schemas, historico
sem resultados de ferramentas e resultados completos das ferramentas. O observador
Pi mede antes de omitir reasoning opaco do payload salvo. Nos artefatos Pi antigos,
a reconstituicao usa payload redigido e os bytes de historico sao parciais; nao
equivalem aos bytes integrais da rede. Bytes nao sao estimativas de tokens.

`daily.py --shuffle-seed N` sorteia a ordem dos cenarios de forma reproduzivel e
mantem a alternancia dos bracos. Com um numero par de rodadas, cada CLI comeca o
mesmo numero de vezes por cenario. A agenda completa e impressa antes das chamadas;
os manifestos registram a rodada. Falhas de braco e de auditoria reprovam a campanha,
preservam sua evidencia e nao interrompem as campanhas seguintes. O pareamento
exige uso completo; o agregado de registros retidos pode incluir uso parcial e
nao substitui a taxa de aprovacao.

Verificacao local do medidor, sem chamadas ao provider:

```powershell
python -B -m unittest discover -s bench/luna-live -p test_measurement.py -v
node --check bench/luna-live/pi-audit.ts
```

Protocolo da nova bateria: quatro rodadas dos seis cenarios `daily.py` e quatro
dos dois cenarios `holdouts.py`, Luna high/normal, sequenciais, seed 20260915.
Os binarios e suas configuracoes de produto permanecem os mesmos da bateria
anterior. Os resultados historicos abaixo ficam preservados.

> O resultado abaixo é o **baseline histórico**, anterior às correções desta
> investigação. A seção [Correções e validação ampliada](#correções-e-validação-ampliada)
> registra as mudanças e todas as rodadas posteriores, sem substituir o baseline.


> **Resultado da etapa de latência, anterior à nova etapa de economia:** No caso original,
> Slim: 122,308 s → média de 21,828 s nas duas execuções finais. Na bateria
> final de quatro cenários e duas repetições, o Slim levou **15,4% menos tempo**
> e venceu 7/8 pares, mas consumiu **22,8% mais tokens**. Isso demonstra vantagem
> nesta amostra com Luna; não garante superioridade universal.


Execução em **05/09/2026**, Windows, mesmo computador e mesma conta Codex
(igualdade verificada localmente, sem registrar credenciais ou identificador da conta).
**Acesso confirmado nos dois por respostas reais e código aprovado.**

## Resultado

| Métrica da tarefa concluída | Pi 0.84.4 | Slim 0.1.0 |
|---|---:|---:|
| Tempo do processo | **30,773 s** | **122,308 s** |
| Chamadas ao modelo / turnos | **5** | **14** |
| Ferramentas executadas | **6** | **18** |
| Ferramentas com falha | 1 | 7 |
| Tokens de entrada, incluindo cache | 9.991 | 72.076 |
| Entrada sem cache | 6.919 | 60.812 |
| Entrada lida do cache | 3.072 | 11.264 |
| Tokens de saída, incluindo reasoning | 886 | 5.410 |
| Reasoning, já incluído na saída | 346 | 2.743 |
| **Total: entrada + saída** | **10.877** | **77.486** |
| Tempo acumulado das chamadas ao modelo | 26,935 s | 116,984 s |
| Soma das durações das ferramentas | 0,386 s | 4,635 s |
| Validação externa | PASS | PASS |
| Código produzido | 37 linhas | 29 linhas |

O Slim levou **3,97×** o tempo e consumiu **7,12×** os tokens nesta amostra.
Isso não é uma estimativa da diferença média entre os produtos. Não se somam
cache e reasoning novamente ao total. Tokens de entrada são acumulados por
requisição: incluem histórico retransmitido, não apenas texto novo do usuário.

## Método e escopo

Uma única tarefa: criar `merge_ranges(ranges)` em Python, validando entradas,
unindo intervalos e preservando o argumento original. A função é pequena;
não há aplicação, framework, dependências ou benchmark com várias tarefas.
O mesmo `SPEC.md`, `check.py` e prompt foram escritos em pastas temporárias
separadas, fora do repositório Slim. O verificador contém quatro entradas
válidas e nove inválidas, mais invariantes de imutabilidade e formato: são
assertions de **uma tarefa**, não treze execuções do modelo.

Ambos usaram **`openai-codex` / `gpt-5.6-luna` / high / normal**, sessões novas,
autenticação OAuth própria e endpoint nativo. Nenhum proxy, resposta simulada
ou provider OpenAI-compatible substituiu a rota Codex. No Pi, desativaram-se
extensões pessoais, skills, templates e arquivos de contexto; uma extensão
somente de observação registrou o payload e eventos, retornando `undefined`.
As ferramentas e instruções nativas foram preservadas. O Slim usou arquivo
de configuração vazio, flags explícitas e seus defaults nativos; a conta
global foi mantida. Não é uma comparação das instalações personalizadas
com todas as extensões do usuário.

O Pi utilizou o transporte configurado `auto`; o Slim usou seu cliente HTTP
Codex. Não foi forçada igualdade de transporte: isso faz parte dos harnesses.
No Pi, payloads observados confirmam modelo, effort e ausência de `service_tier`.
No Slim, flags, adapter e ledger confirmam a configuração aplicada; não houve
captura externa de seu corpo HTTP. As diferenças de schemas e instruções são
deliberadas, pois o objeto do teste é o harness completo.

**Intercorrência preservada:** a primeira tentativa do Pi durou 1,274 s e foi
rejeitada com `Provided authentication token is expired.`, após diagnóstico de
WebSocket e fallback SSE. `pi auth check` havia retornado `ready`; o vencimento
local do JWT ainda estava no futuro. A renovação pelo comando nativo, com
saída da credencial capturada apenas em memória, permitiu concluir a repetição.
Não foi alterada configuração global de modelo, esforço ou transporte.
Essa tentativa não produziu código nem uso informado pelo servidor e está
fora da tabela de tarefas concluídas; não se afirma consumo faturado zero.
O processo Pi terminou com código 0 mesmo nessa falha: os eventos e a
validação externa, não somente o exit code, definem sucesso neste benchmark.

Ordem real, sem sobreposição entre processos medidos: Pi rejeitado → Slim
concluído → Pi renovado e concluído. Não houve uma segunda tarefa nem
repetição bem-sucedida escolhida entre várias amostras.

## Achados para melhorar o Slim

### 1. Contrato de criação de arquivo e erro pouco acionável — prioridade principal

**Verificado:** no turno 2, o modelo pediu `write` para `ranges.py`, ainda
inexistente, com `expected: ""`. A ferramenta respondeu somente erro de I/O
“arquivo especificado” e não criou o arquivo. A chamada não evidencia que
`write` seja incapaz de criar arquivos: a precondição fornecida exige leitura
de um arquivo existente.

O schema torna `expected` opcional e o descreve como necessário para substituir
arquivo existente ([tools/mod.rs](../../crates/slim-core/src/tools/mod.rs),
linhas 1077–1081). O parser preserva string vazia
([execution.rs](../../crates/slim-core/src/tools/execution.rs), 599–602),
o executor a converte em `ExactText` (`tools/mod.rs`, 851–866), e
[write.rs](../../crates/slim-core/src/tools/write.rs), 53–74, tenta ler o alvo
antes de criar. A cadeia explica exatamente a rejeição observada.

**Proposta:** tornar explícito “omita expected para criar” e retornar erro
estruturado/acionável quando houver precondição para arquivo ausente. Uma
eventual mudança de semântica de `expected: ""` deve preservar a distinção
entre arquivo ausente e arquivo existente vazio; não relaxar a proteção
contra sobrescrita. Não foi demonstrado se a presença desse campo veio de
normalização no servidor ou da escolha do modelo.

### 2. Shell do Windows não aparece no contrato e é escolhido por heurística

**Verificado:** depois da rejeição de escrita, o Slim executou `pwd && ls -la
&& find ...` em CMD, que rejeitou `pwd`. Nos turnos seguintes houve três
falhas com PowerShell aninhado, incluindo expansão indevida de `$content`
e tratamento de `False` como comando. A criação só ocorreu no turno 9.

O schema diz apenas “Run a shell command” (`tools/mod.rs`, 1087–1090).
[shell.rs](../../crates/slim-core/src/tools/shell.rs), 139–202, usa CMD por
default no Windows e muda para PowerShell ao encontrar `$`, `::` e outros
marcadores no texto. Isso faz comandos explicitamente aninhados passarem
por interpretação adicional. No Pi, a ferramenta se chama `bash`, seu
prompt identifica Bash e o `ls -la` teve sucesso.

**Proposta:** expor ao modelo o shell realmente executado e estabilizar esse
contrato, evitando inferir o interpretador por caracteres do comando.
Preservar comandos existentes exige validar o comportamento concreto antes
de escolher CMD, PowerShell ou seleção explícita; copiar Bash do Pi não é
pré-requisito nem foi testado como solução.

**Impacto observado conjunto:** os turnos 3–9, entre a escrita recusada e a
criação efetiva, acumularam **61,876 s** de chamadas ao modelo e **32.236
tokens**. São custos observados da recuperação, não economia garantida de
uma correção: um fluxo corrigido ainda precisaria escrever e validar.

### 3. Custo de recuperação domina; reduzir caracteres do prompt é secundário

**Verificado:** 95,65% do tempo total do Slim está no tempo agregado do
provider. As ferramentas somaram 4,635 s; o resíduo entre tempo total e essas
somas foi 0,689 s. Essas somas são contabilidade, não medição isolada de CPU;
ferramentas paralelas podem se sobrepor. No Pi o resíduo foi 3,452 s.

A entrada inicial do Slim foi 1.488 tokens, contra 1.150 do Pi. No último
turno, já era 8.942 contra 2.651. A principal diferença observada é a cadeia
de chamadas com histórico crescente. Diminuir strings do prompt não elimina
as falhas de escrita e shell. Os contadores de bytes de contexto do Slim e
o payload pré-transporte do Pi têm bases diferentes; não são usados como
comparação exata de bytes HTTP ou como substitutos de tokens medidos.

O Slim passou no check no turno 10, depois mudou validação de tipos para
aceitar subclasses, errou um patch por usar aspas diferentes do arquivo,
corrigiu-o e repetiu o check. A alteração pode ser defendida pela leitura
do contrato; não se classifica todo trabalho após o primeiro PASS como
desperdício. O patch rejeitado, por outro lado, está explicitamente registrado.

### 4. Observabilidade de execução interrompida — lacuna confirmada

Durante a execução headless do Slim, o arquivo de sessão continha somente
o cabeçalho. [headless.rs](../../crates/slim-cli/src/headless.rs), 1294–1299,
descarrega e persiste o lote de eventos após o loop terminar. Ao final, o
trace ficou completo e permitiu esta auditoria; uma morte abrupta antes
disso teria perdido os eventos em memória. `SessionEvent` também tem
sequência, sem timestamp próprio ([events.rs](../../crates/slim-core/src/events.rs),
66–70), embora eventos específicos tragam duração.

**Proposta para auditoria:** persistir eventos de fronteira de cada chamada e
ferramenta incrementalmente, com tempo monotônico, sem gravar cada delta de
texto. Isso melhora diagnóstico e contagem de tentativas interrompidas;
não se promete ganho de tokens por essa instrumentação.

## Auditoria por turno

Cache já está incluído em entrada; reasoning já está incluído em saída.
Tempo é o intervalo medido do provider no Slim e entre hooks request/message_end
no Pi, incluindo rede e processamento do stream, não só inferência remota.

### Pi

| Turno | Entrada | Cache | Saída | Reasoning | Provider ms | Ferramentas |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1150 | 0 | 76 | 12 | 3265 | read ×3; arquivo novo ausente |
| 2 | 1714 | 0 | 50 | 15 | 2874 | bash: ls |
| 3 | 1889 | 1536 | 679 | 311 | 15819 | write: criado |
| 4 | 2587 | 0 | 34 | 8 | 2768 | bash: check PASS |
| 5 | 2651 | 1536 | 47 | 0 | 2209 | final |

### Slim

| Turno | Entrada | Cache | Saída | Reasoning | Provider ms | Ferramentas |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1488 | 0 | 145 | 27 | 4365 | list + read ×3; arquivo novo ausente |
| 2 | 2183 | 0 | 587 | 263 | 12057 | write recusado: expected vazio |
| 3 | 2806 | 0 | 135 | 93 | 3686 | shell: pwd rejeitado |
| 4 | 2992 | 0 | 59 | 24 | 2428 | shell: cd/dir |
| 5 | 3400 | 0 | 608 | 574 | 12330 | shell: where powershell |
| 6 | 4052 | 0 | 491 | 143 | 10137 | shell: erro de parsing |
| 7 | 4612 | 0 | 577 | 257 | 11609 | shell: variável expandida |
| 8 | 5336 | 0 | 638 | 316 | 12879 | shell: False inválido |
| 9 | 6107 | 0 | 423 | 101 | 8807 | shell: arquivo criado |
| 10 | 6548 | 0 | 76 | 9 | 2998 | read + shell: check PASS |
| 11 | 6953 | 3584 | 1115 | 829 | 21261 | patch rejeitado: aspas |
| 12 | 8147 | 0 | 303 | 17 | 7067 | patch aplicado |
| 13 | 8510 | 0 | 98 | 31 | 3449 | shell: check PASS + read |
| 14 | 8942 | 7680 | 155 | 59 | 3911 | final |

## Evidências e reprodução

- [Resumo normalizado, todas as chamadas e resultados](summary.json).
- [Slim: sessão completa](20260905-045329Z/slim.session.jsonl),
  [ledger](20260905-045329Z/slim.stdout.jsonl),
  [validação independente](20260905-045329Z/slim.validation.json).
- [Pi concluído: eventos e payloads](20260905-045558Z/pi.audit.jsonl),
  [stdout nativo](20260905-045558Z/pi.stdout.jsonl),
  [validação independente](20260905-045558Z/pi.validation.json).
- [Pi rejeitado: tentativa preservada](20260905-045329Z/pi.stdout.jsonl).
- Cada campanha contém manifesto com versões/hashes, argv, variáveis de
  controle sem credenciais, tempos monotônicos, fixtures e código produzido.

```powershell
# Executa uma tarefa em cada CLI; faz chamadas reais à conta Codex.
python bench/luna-live/run.py
# Recalcula apenas os resultados existentes, sem chamadas ao provider.
python bench/luna-live/analyze.py bench/luna-live/20260905-045558Z bench/luna-live/20260905-045329Z
```

O runner reutiliza `token-economy/v2/process_runner.py`. O analyzer verifica
consistência dos turnos, modelo/effort observados no Pi, usage completo no
Slim, sucesso dos processos e validação independente. O verificador original
é executado por `python -c` fora do `check.py` editável e verifica-se que os
fixtures não mudaram. Os dois códigos também foram lidos nesta auditoria;
ambos validam antes de ordenar uma cópia e têm estrutura O(n log n).

**Limitações:** uma amostra concluída por agente, ordem não randomizada,
host não exclusivo, caches do servidor não controlados e instrumentação
assimétrica. Chamadas são **turnos do modelo**, não conexões TCP: não houve
sniffer de WebSocket/HTTP. O Slim registrou zero retries; o Pi concluído
não registrou erro/diagnóstico de retry. Não se afirma contagem completa
de tentativas de transporte, tráfego de autenticação ou catálogo. Tokens
vieram dos usos informados pelos providers, sem estimativa por caracteres.
Sem preço/custo monetário inferido a partir das tabelas locais dos CLIs.
O ledger Slim marcou `validated_completion=false`; a aprovação aqui é do
oráculo externo, não uma alegação de validação reconhecida pelo runtime.
Na publicação do baseline, as propostas acima ainda não tinham sido implementadas.
A investigação posterior está registrada abaixo.

## ✅ Verificação de Entrega

- [x] Executado `python bench/luna-live/run.py` e depois `run.py pi`: Slim 122308 ms/PASS; Pi renovado 30773 ms/PASS. A tentativa rejeitada foi preservada.
- [x] Executado `analyze.py ...`: Pi 10877 tokens/5 chamadas; Slim 77486 tokens/14 chamadas; assertions passaram.
- [x] Números calculados nesta sessão; arquivos e linhas citados foram lidos.
- [x] `cargo test --workspace`: **não aplicável**, sem alteração de código do produto.
- [x] `refresh-slim.ps1`: **não aplicável**, executável instalado somente observado/executado; sem deploy.
- [x] Documentação do benchmark e índice atualizados; contagens de testes/status do produto não alteradas, pois não houve mudança de comportamento.
- [x] Limitações, tentativa falha e propostas sem validação posterior explicitadas.

## Correções e validação ampliada

Investigação e implementação em 05/09/2026, após o usuário autorizar uma
correção nativa voltada ao uso diário. O baseline preservado acima explica
**61,876 s de recuperação** entre a escrita recusada e a criação efetiva.
Os 116,984 s acumulados no provider não demonstram, por si, servidor lento:
cada decisão para recuperar um erro acrescentou uma chamada e histórico.
Não houve retry do Slim nesse baseline.

### Mudanças de produto

- **Escrita:** `expected` omitido ou `null` cria arquivo novo. Texto exato
  continua obrigatório para substituir um existente; `""` continua significando
  arquivo existente vazio. A rejeição de precondição sobre arquivo ausente
  informa como repetir a operação corretamente. Não há sobrescrita silenciosa.
- **Shell:** PowerShell declarado e estável, sem escolher CMD/PowerShell pela
  presença de `$` ou `::`. CMD/Bash continuam possíveis por invocação explícita.
  O código de saída de programas nativos é preservado. A mudança de default
  exige sintaxe PowerShell em consumidores que antes dependiam implicitamente
  de CMD; os dois testes existentes com comandos CMD foram adaptados mantendo
  suas assertions de falha e efeitos laterais.
- **Recuperação de leitura:** arquivo ausente identifica o caminho e orienta
  criação nativa, preservando o erro. Não esconde falhas nem retorna leitura vazia.
- **Estado local:** `.slim/artifacts` só é criada quando há conteúdo a
  externalizar. A listagem continua mostrando pastas reais; não há filtro
  especial de benchmark. A descrição de `list` identifica o papel de `.slim`.
- **Fluxo nativo:** usar caminhos fornecidos, agrupar leituras independentes,
  validar o requisito e terminar quando atendido. Preserva testes necessários,
  proteção do trabalho, instruções de projeto e conclusão de pendências reais.
- **Schemas Responses:** `strict:false` explícito preserva argumentos opcionais.
  O contrato local valida os argumentos antes de executar ferramentas.
- **Verbosidade GPT:** `low` nos modelos GPT conhecidos pelo catálogo, no
  adapter Responses e no endpoint oficial OpenAI Chat Completions. Não altera
  esforço de reasoning, limite de saída, modelo ou velocidade Normal/Fast.
  Não envia esse controle a modelos sem suporte conhecido.

As primeiras cinco mudanças são compartilhadas pelo produto; os ajustes de
serialização respeitam os protocolos. O teste de contratos percorre sete
rotas de provider, em dez variantes Chat/Messages/Responses. Isso confirma
wiring e execução local, **não** mede ganho de latência em todos os providers.

### Diferença descoberta nos payloads

Os payloads auditados do Pi enviam `text.verbosity: low` e `strict:null` nas
funções; o Slim anterior omitia ambos. Portanto, modelo/effort/Normal iguais
**não significavam verbosidade igual** nas tabelas históricas. A API documenta
`medium` como default e `low` como orientação de concisão, ainda subordinada ao
conteúdo solicitado. [Guia oficial de modelos](https://developers.openai.com/api/docs/guides/latest-model).

A documentação Responses explica que omitir `strict` permite normalização
para schema estrito, incluindo tornar campos opcionais obrigatórios, e indica
`strict:false` para manter execução best effort.
[Function calling oficial](https://developers.openai.com/api/docs/guides/function-calling).
A possibilidade de normalização explica por que declarar explicitamente esse
contrato é necessário; **não prova que ela causou o `expected:""` daquela
requisição original**. Não houve ablação isolada para atribuir segundos a cada
uma dessas mudanças.

O significado do flag não é universal: a xAI documenta conformidade estrita
sempre ativa e mostra campos opcionais em seus exemplos. O teste de adapter
confirma os campos enviados pelo Slim; não afirma desativar a política do
servidor xAI. [Contrato xAI](https://docs.x.ai/developers/tools/function-calling),
[structured outputs xAI](https://docs.x.ai/developers/model-capabilities/text/structured-outputs).

### Método ampliado e rodadas intermediárias

`daily.py` executa quatro cenários fixos, duas vezes cada, alternando a ordem:
criação Python (`merge_ranges`), correção Python com chamador existente
(`repair_catalog`), CLI JSON com caminhos Unicode/espaços e erros de entrada
(`json_cli`), e correção CommonJS de paginação/imutabilidade (`js_pagination`).
O cenário JavaScript foi definido antes de avaliar o fluxo v1.1. Os testes
externos e SPEC são os mesmos para os dois braços, em workspaces novos.
Nenhuma redução de effort ou troca de provider foi usada.

Não é uma amostra representativa de grandes repositórios. Os testes verificam
comportamentos pedidos e preservação de fixtures; não provam qualidade geral,
segurança universal ou superioridade em tarefas de longa duração.

| Etapa completa | Pares | Slim total | Pi total | Chamadas Slim/Pi | Falhas de ferramenta Slim/Pi | Tokens Slim/Pi |
|---|---:|---:|---:|---:|---:|---:|
| Escrita e shell, prompt anterior | 6 | 252,654 s | 188,299 s | 37/30 | 1/2 | 115.429/69.181 |
| Mais fluxo nativo v1.1 | 8 | 288,821 s | 238,938 s | 44/43 | 2/1 | 129.135/98.728 |

A primeira etapa contém os três cenários Python. A segunda acrescenta
JavaScript: os totais de etapas com escopos diferentes **não são comparação
longitudinal direta**. Dentro de cada etapa, ambos executaram tarefas idênticas.
Todos os pares passaram no oráculo externo. Restaram leituras de arquivo ainda
inexistente nas falhas Slim, sem a cadeia original de escrita/shell quebrados.
O Slim ainda levou 34,2% e 20,9% mais tempo, respectivamente. Esses resultados
intermediários motivaram a investigação de estado local e payloads; não foram
omitidos por serem desfavoráveis.

Campanhas da etapa escrita/shell: `20260905-060209Z`,
`20260905-060324Z-repair_catalog`, `20260905-060405Z-json_cli`,
`20260905-060632Z`, `20260905-060817Z-repair_catalog`,
`20260905-060856Z-json_cli`.

Campanhas da etapa fluxo v1.1: `20260905-061820Z`,
`20260905-061915Z-repair_catalog`, `20260905-061956Z-json_cli`,
`20260905-062157Z-js_pagination`, `20260905-062244Z`,
`20260905-062345Z-repair_catalog`, `20260905-062426Z-json_cli`,
`20260905-062627Z-js_pagination`.
Cada pasta preserva manifest, tempos, sessões, código produzido, validação e
`summary.json` com todas as chamadas, sem escolher a melhor repetição.

**Correções do medidor:** as campanhas `060405Z-json_cli` e `060632Z`
registraram falsos desacordos de fixtures por cp1252/UTF-8 e LF/CRLF,
respectivamente. A checagem passou a comparar os bytes da referência salva
com os bytes no workspace. As validações iniciais foram preservadas em
`initial_validation`; o oráculo externo original foi reexecutado sobre as
cópias salvas. Não houve nova geração pelo modelo para substituir esses casos.

### Rodada com ferramentas, estado lazy e payload alinhado

Build `D6B841E19641C8A6B97AB2B62B622CAE15A20377891ECB994B3910BEB8C03C6D`,
15.038.464 bytes, implantado às 03:42:23. Mesmos quatro cenários e duas
repetições, todas concluídas; todos os oráculos passaram e fixtures preservadas.

| Cenário/repetição | Slim | Pi | Tokens Slim/Pi |
|---|---:|---:|---:|
| merge_ranges 1 | 22,131 s | 23,619 s | 10.719/10.396 |
| repair_catalog 1 | 19,872 s | 21,575 s | 11.911/10.265 |
| json_cli 1 | 69,483 s | 39,218 s | 37.030/13.889 |
| js_pagination 1 | 18,030 s | 19,916 s | 12.436/10.371 |
| merge_ranges 2 | 26,334 s | 25,989 s | 10.912/10.633 |
| repair_catalog 2 | 18,235 s | 20,623 s | 11.938/9.723 |
| json_cli 2 | 46,613 s | 51,818 s | 18.739/14.051 |
| js_pagination 2 | 22,231 s | 23,810 s | 13.184/10.268 |
| **Total** | **242,929 s** | **226,568 s** | **126.869/89.596** |

Slim mais rápido em 6/8 pares, mas ainda **7,2% mais lento no total** e com
41,6% mais tokens. Não se declara superioridade pelo número de vitórias.
Chamadas: 43/40; ferramentas: 59/53; falhas: 4/3. As quatro falhas Slim
foram leituras/listagens de alvos ainda ausentes; **zero falhas de write/shell**.

Na primeira CLI JSON, o Slim gastou três turnos em duas mudanças de imports e
nova validação, depois do PASS. O loop não injetou um pedido de nova revisão:
foram decisões do modelo. A segunda CLI não teve esse mesmo polimento.
O ajuste seguinte do prompt v1.2 esclarece que a regra de evitar mudanças
puramente cosméticas também se aplica ao código recém-escrito, respeitando
exigências do usuário/projeto e correções necessárias. Não introduz interrupção
automática após qualquer shell com exit 0, pois isso poderia encerrar trabalho
incompleto e não comprova correção.

Campanhas: `20260905-064329Z`, `20260905-064415Z-repair_catalog`,
`20260905-064457Z-json_cli`, `20260905-064647Z-js_pagination`,
`20260905-064726Z`, `20260905-064819Z-repair_catalog`,
`20260905-064858Z-json_cli`, `20260905-065038Z-js_pagination`.

### Validação do produto após o ajuste v1.2

`refresh-slim.ps1 -Test` terminou com exit 0: `cargo test --workspace`
registrou **1119 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler
warnings**, seguido de build release e smoke de versão. Clippy workspace
com `--all-targets -- -D warnings` terminou com exit 0. `git diff --check`
e os links locais deste relatório passaram. Não foi aplicado formatter global
sobre o WIP preexistente; os arquivos novos de teste e os fontes de leitura e
artefatos alterados foram formatados diretamente.

Saída do deploy:

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 03:57:28)
```

`target/release/slim.exe` e o executável do PATH têm 15.038.464 bytes e
SHA-256 **23AF84BDB9461630B445680136D555D6A0B8B1BBA2F145EFDFB98AD7AEBAA317**.
`slim --version`: `slim 0.1.0`, exit 0. Logs desta sessão:
`%TEMP%/slim-native-v12-gate.log` e `%TEMP%/slim-native-v12-clippy.log`.

A primeira tentativa paralela do gate anterior falhou na linkagem MSVC por
memória virtual insuficiente (`LNK1171`, `mspdbcore.dll`, erro 1455). O gate
foi concluído com `CARGO_BUILD_JOBS=1`, sem descartar alterações ou limpar o
checkout. Uma compilação intermediária foi interrompida deliberadamente antes
de release para incorporar os controles de API; não é contada como aprovação.

O teste físico ConPTY continua ignorado; não foi feita validação visual de
console físico, geração do ZIP de distribuição, commit, push ou publicação.
A persistência incremental de eventos headless continua uma lacuna separada:
esta tarefa corrigiu execução/desempenho, e os traces completos das campanhas
concluídas permitem a auditoria, mas não resolvem perda em morte abrupta.

### Rodada v1.2: menos chamadas, resumo final ainda caro

O ajuste v1.2 foi medido em mais oito pares completos, todos aprovados, com
fixtures preservadas. Resultados médios das duas repetições por cenário:

| Cenário | Slim | Pi |
|---|---:|---:|
| merge_ranges | 22,909 s | 26,208 s |
| repair_catalog | 20,966 s | 21,344 s |
| json_cli | 57,869 s | 41,493 s |
| js_pagination | 20,661 s | 23,211 s |
| **Total das oito execuções** | **244,808 s** | **224,510 s** |

Slim mais rápido em 5/8 pares e em três médias de cenário, mas **9,0% mais
lento na soma**. Mediana: 21,922 s/24,735 s; esse indicador não substitui o
resultado desfavorável da soma. Tokens: 113.807/88.731; chamadas: 40/39;
ferramentas: 56/54; falhas: 3/3. As falhas Slim foram duas leituras e uma
listagem de alvos inexistentes, com zero falhas de escrita/shell.

As CLIs JSON não fizeram patches após o PASS, mas seus turnos finais duraram
11,737 s e 11,783 s, com 512 e 516 tokens de reasoning antes de resumos curtos.
Os turnos finais Pi dos mesmos pares duraram 2,887 s e 2,132 s, com zero
reasoning informado. Essa é uma diferença observada no encerramento, não um
travamento de execução das ferramentas. A validade externa não depende do
resumo do agente. O ajuste v1.3 delimita o resumo aos resultados observados e
limitações conhecidas, sem pedir outra revisão ou busca de riscos hipotéticos.
Seu efeito precisa de medição posterior; não se deduz economia garantida
simplesmente subtraindo esses tempos.

Campanhas: `20260905-065807Z`, `20260905-065857Z-repair_catalog`,
`20260905-065937Z-json_cli`, `20260905-070120Z-js_pagination`,
`20260905-070205Z`, `20260905-070253Z-repair_catalog`,
`20260905-070339Z-json_cli`, `20260905-070518Z-js_pagination`.

### Experimento v1.3 descartado

O esclarecimento adicional do resumo final não mostrou benefício consistente.
A rodada completa registrou Slim **327,490 s**, Pi **272,156 s** (Slim +20,3%),
com todos os oito pares aprovados e fixtures preservadas. Chamadas: 54/44;
ferramentas: 69/54; falhas: 4/3; tokens: 180.119/106.493.
Além das leituras de arquivo ausente, houve uma precondição de patch rejeitada
e uma transição inválida de todo. Dois cenários simples passaram a usar listas
de tarefas, adicionando chamadas sem ganho demonstrado no resultado solicitado.

Não se atribui causalidade definitiva a uma frase por oito amostras estocásticas.
Contudo, não há evidência suficiente para manter esse ajuste adicional: a única
mudança v1.3 foi revertida, incluindo seu marcador de cache, preservando o
prompt v1.2 e todas as correções de ferramentas/protocolo. Os números v1.3
permanecem aqui; não são usados como desempenho do executável final restaurado.

Um patch após o PASS na primeira CLI JSON tratou UTF-8 inválido, requisito real
do contrato. Portanto, parte do trabalho adicional foi correção necessária,
e não simples polimento. Na mesma rodada, o turno final do Pi também chegou a
11,310 s com 516 tokens de reasoning. Isso limita a hipótese de que o custo do
resumo viesse exclusivamente da instrução nativa do Slim. Não foi introduzida
uma parada automática após exit 0 nem reduzido esforço para favorecer a medição.

Campanhas: `20260905-071421Z`, `20260905-071513Z-repair_catalog`,
`20260905-071604Z-json_cli`, `20260905-071839Z-js_pagination`,
`20260905-071922Z`, `20260905-072006Z-repair_catalog`,
`20260905-072049Z-json_cli`, `20260905-072325Z-js_pagination`.

### Conclusão intermediária, antes da correção de todo

As causas mecânicas da cadeia original foram corrigidas: criação de arquivo e
shell não falharam na bateria v1.2; erro de leitura ausente orienta recuperação.
No caso original, Slim passou de **122,308 s para média de 22,909 s** nas duas
execuções v1.2 (Pi: 26,208 s), com 4 chamadas ao modelo em cada execução.

**A paridade geral ainda não foi atingida:** a bateria v1.2 preservada no produto
ficou 9,0% atrás do Pi no total, principalmente nas duas CLIs JSON; teve 28,3%
mais tokens. Melhor desempenho mediano ou em três dos quatro cenários não
anula essa diferença. Os testes locais confirmam funcionamento e contratos
compartilhados; não provam igualdade de latência em todos os providers.
Nenhuma alegação de objetivo geral concluído é sustentada por estes dados.

### Estado local entregue após a reversão isolada

O código do prompt voltou ao v1.2: apenas a frase adicional v1.3 e seu marcador
de cache foram revertidos; nenhuma ferramenta ou ajuste de protocolo foi
retirado. O gate foi executado novamente, sem filtrar testes:

- `refresh-slim.ps1 -Test`: exit 0; **1119 passed / 0 failed / 1 ignored /
  73 suítes / 0 compiler warnings**; release e smoke de versão aprovados.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0.
- `git diff --check`: exit 0; links locais conferidos; estados atuais dos
  índices, release, plano, design e tracker atualizados, preservando histórico.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 04:32:01)
```

Target e PATH: **15.038.464 bytes**, SHA-256 idêntico
**C115056FD460771B32775392DA812E2CFA80B455EB8874D1CA6C759F3D87E16C**.
A recompilação gerou hash diferente do build v1.2 medido anteriormente.
A bateria de desempenho citada corresponde ao código v1.2 antes dessa
recompilação; não foi executada novamente no binário final. A restauração
isolada recompôs o mesmo texto/contrato, e os testes foram repetidos, mas não
se afirma identidade binária entre os dois builds. Logs finais:
`%TEMP%/slim-native-delivery-gate.log` e
`%TEMP%/slim-native-delivery-clippy.log`.

#### Checklist de entrega desta correção

- [x] Comandos executados e saídas registrados nesta sessão.
- [x] Contagens e medições recalculadas, sem reutilizar status histórico como gate.
- [x] Fontes, chamadores, contratos e caminhos referenciados lidos.
- [x] `cargo test --workspace` verde, 0 falhas e 0 compiler warnings.
- [x] `refresh-slim.ps1 -Test` executado e `OK:` confirmado.
- [x] Documentos de status atualizados; números antigos só em registros históricos.
- [x] Limites declarados: paridade geral não atingida, benchmark live em Codex/Luna,
  ConPTY físico ignorado; sem validação visual, publicação ou ZIP novo.

## Correção dos IDs e status de todo

A continuação da investigação encontrou um defeito determinístico além da
variação do modelo. No trace `20260905-072325Z-js_pagination`, a ferramenta
aceitava IDs nos argumentos, mas o parser os descartava antes de construir
`TaskMutation::TodoSetStatus`. A bridge alterava o primeiro item pendente ou o
último; sua projeção durável alterava o último. A criação também ignorava
`status`, e as respostas não informavam os IDs alocados. Portanto, os rótulos
`todo updated: todo N` não provavam que o item N havia sido atualizado.

A reprodução no handler usado pelo loop falhou antes da correção:
`Pending != InProgress` para uma criação explicitamente solicitada como
`in_progress`. A primeira versão da fixture usava a API de ferramentas simples,
que não despacha `todo`; ela foi corrigida para exercitar o handler real antes
de atribuir a falha ao produto.

Mudança nativa, sem condição de provider ou cenário:

- O ID e o status inicial agora atravessam parser, ledger e bridge. Atualizações
  que também trazem um título continuam sendo atualizações do ID, sem criar
  outro item silenciosamente. As respostas informam ID, status e título reais.
- O schema aceita os IDs string/inteiro que o parser já suporta e explica quando
  usar acompanhamento, como agrupar transições e o limite de um item em andamento.
- Entradas sintaticamente inválidas em lote são rejeitadas antes de aplicar
  qualquer uma. Falhas semânticas posteriores informam as entradas já aplicadas
  e o estado atual; a TUI recebe essa mudança parcial, sem sucesso falso.
- Os novos campos duráveis são opcionais na desserialização. Registros antigos
  continuam legíveis; a seleção histórica sem ID segue a regra da bridge antiga,
  agora também usada pela projeção do ledger. Registros novos carregam o alvo.
- É permitido adicionar itens pendentes ou concluídos enquanto outro está em
  andamento; iniciar um segundo item em paralelo continua proibido.

Os testes focados verificaram IDs, status inicial, alvo fora de ordem, criação
sem duplicação em updates com título, dados inválidos, conflito de andamento,
erro parcial publicado e retomada da bridge com registros antigos e novos.
A fixture HTTP existente também confirma que os IDs chegam ao próximo request
do modelo. Clippy passou antes do gate completo.

A correção funcional elimina alteração do item errado e a necessidade de
adivinhar IDs. A bateria final abaixo mede este executável; os tempos das
seções anteriores permanecem como histórico das versões anteriores. A retomada testada aqui
é da bridge durável, não uma alegação de cobertura completa de conversa longa
ou retomada visual da TUI.

Validação integral desta correção: `refresh-slim.ps1 -Test` terminou com exit 0,
**1122 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings**.
`cargo clippy --workspace --all-targets -- -D warnings` e `git diff --check`
passaram. O script imprimiu:

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 05:05:32)
```

Target e PATH: **15.046.656 bytes**, SHA-256 idêntico
`E9DC086A42F658850B4718A9A70DDEBD88042650B8B65CB2AB5D9FC260E7C590`, `slim --version` =
`slim 0.1.0`. Logs locais: `%TEMP%/slim-todo-gate.log`,
`%TEMP%/slim-todo-clippy.log` e `%TEMP%/slim-todo-wire.log`.

## Bateria final com a correção de todo

Comando: `python bench/luna-live/daily.py --rounds 2`, exit 0. Mesmos quatro
cenários e oráculos, duas repetições, ordem alternada. Codex nativo, Luna,
`high`, velocidade normal e sem proxy; nenhum caso excluído ou repetido para
substituir um resultado. Os oito manifests identificam o SHA-256 instalado
`E9DC086A42F658850B4718A9A70DDEBD88042650B8B65CB2AB5D9FC260E7C590`.

| Rodada | Cenário / evidência | Slim | Pi |
|---|---|---:|---:|
| 1 | [merge_ranges](20260905-080555Z/summary.json) | 21.915 s | 31.440 s |
| 1 | [repair_catalog](20260905-080649Z-repair_catalog/summary.json) | 19.933 s | 24.460 s |
| 1 | [json_cli](20260905-080734Z-json_cli/summary.json) | 47.468 s | 51.193 s |
| 1 | [js_pagination](20260905-080915Z-js_pagination/summary.json) | 19.063 s | 28.997 s |
| 2 | [merge_ranges](20260905-081003Z/summary.json) | 21.741 s | 24.081 s |
| 2 | [repair_catalog](20260905-081049Z-repair_catalog/summary.json) | 22.658 s | 22.263 s |
| 2 | [json_cli](20260905-081135Z-json_cli/summary.json) | 45.326 s | 51.227 s |
| 2 | [js_pagination](20260905-081313Z-js_pagination/summary.json) | 21.677 s | 26.224 s |

| Agregado de oito tarefas | Slim | Pi |
|---|---:|---:|
| Tempo total | 219.781 s | 259.885 s |
| Tempo nos requests ao provider | 215.703 s | 227.547 s |
| Chamadas ao modelo | 40 | 41 |
| Chamadas de ferramentas | 56 | 53 |
| Falhas de ferramentas | 3 | 3 |
| Tokens totais | 114228 | 93001 |
| Tokens de saída | 8658 | 7704 |
| Tokens de reasoning | 3498 | 2903 |

**Resultado:** 219,781 s contra 259,885 s, redução de **15,4%** no tempo total;
Slim mais rápido em **7/8 pares**. No cenário original, a média final do Slim
é **21,828 s**, contra 122,308 s no baseline histórico (redução observada de
82,2%; não uma ablação causal). Todos os processos terminaram com exit 0,
passaram no oráculo externo original e preservaram `SPEC.md` e `check.py`.
A auditoria dos oito pares (`analyze.py`) passou sem resultado incompleto.

No Slim, as três falhas foram duas leituras do arquivo ainda inexistente e uma
listagem de diretório ainda inexistente. Não houve falha de criação/escrita
ou shell. O tempo total inclui inicialização das CLIs: a diferença nos
requests ao provider foi menor, **215,703 s contra 227,547 s**. Logo, a vantagem
total não deve ser apresentada integralmente como ganho no modelo.

**Limites:** são oito pares com um modelo comercial e variação estocástica.
A comparação anterior v1.2 foi 9,0% mais lenta no agregado; esta rodada não
isola o efeito causal da correção de `todo`. Não se conclui vantagem constante
em toda tarefa, conversa longa ou provider. O Slim ainda consumiu **22,8%
mais tokens** (114.228 contra 93.001); velocidade e economia não são equivalentes.
A entrega corrigiu os defeitos nativos reproduzidos e alcançou vantagem na
bateria final ampliada, sem tratar essa amostra como garantia universal.

## Economia transversal — prefixo compartilhado

Nova meta: consumir menos tokens que o Pi de maneira ampla, preservando qualidade.
A amostra anterior tinha 114.228 tokens no Slim e 93.001 no Pi. Esta etapa não
trata uma vitória em tarefas curtas com um único modelo como prova universal.
Código e medições são as fontes primárias; relatórios anteriores são histórico.

No par `20260905-081049Z-repair_catalog`, ambos fizeram cinco chamadas, mas cada
request do Slim carregava 6.798 bytes de schemas, contra 2.845 no Pi. Os sistemas
serializados eram semelhantes (2.653 e 2.695 bytes, respectivamente; o observador
Pi mede a string sem o envelope JSON). Isso direcionou a primeira mudança ao
prefixo repetido, preservando ferramentas e informações retornadas.

- Prompt nativo consolidado: **2.641 → 1.788 bytes UTF-8** (antes da serialização).
  Autoridade, permissões, preservação de trabalho, qualidade, evidência e
  conclusão após validação continuam explícitos.
- Descrições retiram repetição entre explicação geral e campos. Tipos, limites,
  precondições e execução das ferramentas de arquivo/shell permanecem iguais.
- `todo` anuncia apenas `todos:[...]` com título/ID/status, evitando duplicar o
  formulário em dois níveis e anunciar aliases. Parser continua aceitando as
  chamadas anteriores; IDs/status e falhas parciais seguem cobertos pelos testes.
- Não houve redução de effort, teto de saída, conteúdo lido, histórico ou testes.
  Não foi adicionado provider/modelo condicional à economia do prefixo.

A fixture de contratos foi ampliada para incluir todo/skill junto das ferramentas
nativas nos sete providers/dez variantes de protocolo. O teste de todo confirma a
forma anunciada e executa criação/atualização reais pelo handler do loop.
O gate `refresh-slim.ps1 -Test` terminou com **1122 passed / 0 failed / 1 ignored /
73 suítes / 0 compiler warnings**, exit 0. Clippy com `-D warnings` também passou.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 05:35:33)
```

Target/PATH: **15.041.024 bytes**, SHA-256 idêntico
`3DA704675A3615F23BA0097EDC259BA5945CE32CA53B2AAFB0B482E3ED5D8D2C`.
Logs: `%TEMP%/slim-economy-prefix-focused.log`, `slim-economy-prefix-gate.log`,
`slim-economy-prefix-clippy.log`. Bytes de prefixo não são tokens faturados;
resultado comercial será registrado abaixo quando a bateria terminar.

O runner agora aceita provider/modelo, preservando seus defaults e a exigência
`high` no auditor. `--normal` permanece exclusivo do Codex. O observador Pi
reconhece também mensagens Chat, sem registrar cabeçalhos ou credenciais.
Há configuração OpenCode Go nos dois clientes; isso permite testar outro modelo
por sua rota nativa, sem migrar o produto ou reaproveitar respostas gravadas.

### Primeira bateria de economia: Luna/Codex

`python bench/luna-live/daily.py --rounds 2` terminou com exit 0.
Oito pares, todos preservados e auditados; oráculos externos e fixtures intactos.

| Rodada | Cenário / evidência | Tokens Slim | Tokens Pi | Calls Slim/Pi |
|---|---|---:|---:|---:|
| 1 | [merge_ranges](20260905-083619Z/summary.json) | 9486 | 8884 | 4/4 |
| 1 | [repair_catalog](20260905-083709Z-repair_catalog/summary.json) | 10749 | 11293 | 5/7 |
| 1 | [json_cli](20260905-083756Z-json_cli/summary.json) | 25795 | 12390 | 8/5 |
| 1 | [js_pagination](20260905-083938Z-js_pagination/summary.json) | 11620 | 10701 | 5/5 |
| 2 | [merge_ranges](20260905-084031Z/summary.json) | 11008 | 8937 | 5/4 |
| 2 | [repair_catalog](20260905-084124Z-repair_catalog/summary.json) | 17290 | 9895 | 7/5 |
| 2 | [json_cli](20260905-084218Z-json_cli/summary.json) | 17661 | 21267 | 6/7 |
| 2 | [js_pagination](20260905-084418Z-js_pagination/summary.json) | 11046 | 12772 | 5/6 |

**A meta ainda não foi atingida:** Slim 114,655 tokens, Pi 96,139; diferença de 19.3%.
O primeiro request de repair_catalog caiu de 1.709 para 1.262 tokens, com
system/schema serializados passando de 2.653/6.798 para 1.799/5.387 bytes. A
medida de schemas do Runtime sem backend LSP (4.149 bytes) não representa o
catálogo da CLI: ela conecta o backend Rust e anuncia code_intel também nestes
workspaces Python/JS. O catálogo efetivamente enviado é a medida relevante.

A redução do prefixo não garantiu redução do custo completo: houve mais rodadas.
No `20260905-083756Z-json_cli`, duas patches trocaram __import__('sys') por um
import normal antes da validação; é retrabalho de estilo. Em
`20260905-084124Z-repair_catalog`, write recebeu expected com LF para conteúdo
CRLF apresentado por read sem os terminadores. O guard rejeitou como stale;
o modelo releu e recorreu a patch. Patch já normaliza LF→CRLF; write exige
igualdade integral de bytes. A inspeção de read.rs/write.rs/patch.rs confirmou
a diferença. Não se atribui causalidade estatística ao novo prompt com esta
amostra; estes traces identificam trabalho concreto a reduzir.

Próximos pontos fundamentados: evitar repetição integral do arquivo em guards
de escrita sem perder a verificação de alterações concorrentes; adequar o
catálogo à disponibilidade real das capacidades. Nenhum desses mecanismos
foi implementado ou declarado validado nesta primeira etapa.

### DeepSeek/OpenCode Go: falha real de streaming

A campanha `20260905-084558Z-repair_catalog` usou `opencode-go`/
`deepseek-v4-flash`, high, nos dois CLIs nativos. Pi concluiu e passou no
oráculo; Slim terminou com exit 21, `provider_error: provider returned a
malformed tool call`, sem executar ferramenta. Seus 2.198 tokens consumidos
não representam uma tarefa concluída nem entram numa comparação de economia.
O par falho foi preservado; não se substitui o resultado por uma repetição.

Uma captura diagnóstica direta no mesmo endpoint, usando o prompt/payload da
fixture gravada e a autenticação local do Slim, confirmou o formato emitido:
primeiro fragmento com ID string; seguintes com o mesmo índice e `id:null`,
`name:null`. O arquivo [protocol-probe.sse](20260905-084558Z-repair_catalog/protocol-probe.sse)
contém a resposta sem headers/credenciais. Isso foi diagnóstico, não uma
execução equivalente dos agentes. A tentativa inicial com urllib recebeu 403;
Node fetch conseguiu capturar o stream. Nenhum proxy foi introduzido.

O parser Chat compartilhado tratava todo ID presente que não fosse string como
chamada malformada, incluindo null. A correção trata null como ID ausente;
o índice continua ligando o fragmento à chamada original. IDs incompatíveis,
tipos inválidos e argumentos incompletos continuam sujeitos aos guards existentes.
Não há condição por DeepSeek ou OpenCode Go nessa correção.

A fixture HTTP existente de duas chamadas fragmentadas passou a incluir null
numa continuação e omissão na outra. Antes da correção: RED com
`MalformedToolCall` (exit 101). Após: suites de provider_http, provider_adapters
e agent_loop GREEN, preservando IDs/nomes/argumentos completos e distintos.
Clippy com `-D warnings` passou. Logs: `%TEMP%/slim-null-id-red.log`,
`slim-null-id-green.log` e `slim-null-id-clippy.log`.

Gate integral após a correção de streaming: **1122 passed / 0 failed / 1 ignored /
73 suítes / 0 compiler warnings**, `refresh-slim.ps1 -Test` exit 0, Clippy exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 05:56:49)
```

Target/PATH: 15.041.024 bytes, SHA-256 idêntico
`FF652E7CFC09D8B334A1AC9222EBAAF139319A1A2357F3503B638DCF83E68CA4`.
O patch de streaming não altera o payload/fluxo Responses usado na bateria Luna
anterior; ainda assim, aqueles manifests identificam o executável anterior,
e não este hash recompilado.


### DeepSeek após a correção do parser

Duas novas execuções nativas de repair_catalog concluíram e passaram nos oráculos
com fixtures intactos (`slim-null-id-deepseek.log`, exit 0):

| Evidência | Tokens Slim/Pi | Calls Slim/Pi | Falhas de ferramenta Slim/Pi |
|---|---:|---:|---:|
| [085715](20260905-085715Z-repair_catalog/summary.json) | 18414 / 14315 | 6 / 5 | 1 / 0 |
| [085750](20260905-085750Z-repair_catalog/summary.json) | 18147 / 15338 | 6 / 5 | 1 / 0 |

Total: 36561 / 29653 tokens, Slim 23,3% acima. Nas duas, write foi chamado sem
expected depois de read e rejeitado por falta de precondição. O modelo recuperou
com patch. Isso fundamentou a próxima mudança no contrato nativo de escrita.

### Guarda de escrita baseada na leitura

Read passa a guardar SHA-256 dos bytes brutos efetivamente lidos, antes da
numeração/normalização de apresentação. Uma leitura completa, inclusive páginas
consecutivas desde a primeira até EOF, permite omitir expected na escrita.
O writer compara o digest com o arquivo atual usando a guarda existente antes
da substituição. Leitura parcial, outro arquivo ou alteração externa não autorizam
o overwrite; expected explícito incorreto continua sendo rejeitado.

O estado fica no cache limitado de leitura (32 arquivos), sem duplicar conteúdo
persistido ou adicionar campos ao payload do modelo. Escrita/patch invalidam a
observação anterior. Uma falha de precondição invalida também a evidência em cache,
permitindo releitura atualizada. CRLF uniforme é preservado e expected com LF é
aceito nesse caso, como já ocorria em patch. Terminadores mistos não recebem essa
normalização. O limite de 10 MiB é conferido também depois da conversão para CRLF.

A regressão de overwrite após read falhou antes (exit 101), passou depois. Os
casos exercitam leitura inteira/paginada/parcial, Unicode, arquivo diferente,
expected incorreto, alteração externa e nova leitura. O teste de limite de escrita
inclui expansão LF→CRLF acima do limite e verifica arquivo intacto e ausência de
resíduo temporário. Os testes focados e Clippy com -D warnings passaram; o gate e
os benchmarks da nova versão serão registrados com o hash efetivamente executado.

Antes de medir essa versão, foram fixados dois cenários adicionais: ledger_audit
(auditoria CSV com versões, status, reembolsos e nomes especiais) e config_migration
(múltiplos JSONs, preservação de campos aninhados e arquivo já atualizado). Os
oráculos rejeitaram as fixtures iniciais e aceitaram soluções locais conhecidas.
Esses cenários foram adicionados sem remover os quatro anteriores.


**Correção de comparabilidade DeepSeek:** a inspeção posterior do payload Pi
confirmou `thinking:{type:enabled}` além de `reasoning_effort:high`. O adapter Chat
do Slim enviava apenas o segundo campo e não guardava reasoning_content para
continuação. Portanto, os pares DeepSeek anteriores NÃO comprovam economia em
condições equivalentes de raciocínio, mesmo quando os oráculos passaram. Eles
continuam úteis como reprodução das falhas de streaming/write. O auditor agora
exige thinking ativado no Pi e evidência de raciocínio efetivo no stream Slim.
O contrato completo do V4 exige ativação e retorno do estado nas chamadas seguintes:
[documentação primária DeepSeek](https://api-docs.deepseek.com/guides/thinking_mode/).

Gate da guarda de leitura: `refresh-slim.ps1 -Test`, exit 0, 1124 passed, 0 failed,
1 ignored, 73 suítes, 0 compiler warnings. Clippy com -D warnings, exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 06:26:16)
```

Target/PATH: 15048192 bytes, SHA-256 idêntico
`DF12417AECA59556430A439B2D1D4DEAA71015E7EEC532F110B6F6A8AC77CB64`.
Logs: `%TEMP%/slim-observed-write-gate.log` e `slim-observed-write-clippy.log`.

O par `20260905-092633Z-repair_catalog` falhou por transporte no terceiro request
Slim: 15002 ms, nenhum byte recebido nessa chamada, usage desconhecido e exit 21.
Nenhuma escrita chegou a ser chamada; fixtures intactos. A segunda rodada prevista,
`20260905-092859Z-repair_catalog`, concluiu com oráculos PASS nos dois braços,
14572/15191 tokens Slim/Pi e 5/5 calls, sem falhas de ferramenta. Essa vantagem
não é contada como vitória equivalente devido à divergência de thinking acima.


### Catálogo por disponibilidade do workspace

O runtime reavalia o catálogo antes de cada request. O backend LSP só anuncia
code_intel quando consegue servir o workspace; descoberta não inicia servidor.
O teste HTTP alterna disponibilidade através de alterações feitas pelo próprio
loop e verifica os schemas nos requests seguintes. O teste LSP real de descoberta
cobre ausência/criação/remoção de Cargo.toml com zero processos iniciados; shutdown
continua sendo respeitado. Backends sem restrição mantêm o default anunciado.

Gate: 1126 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings.
Clippy -D warnings e refresh-slim.ps1 -Test: exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 06:35:59)
```

15052288 bytes; SHA-256 target/PATH
`3FBFA600337A8F29744DB3292DABBB675CF1C195C8C39EE47C611E6EBED58400`.
Logs: `%TEMP%/slim-workspace-catalog-focused.log`, `slim-workspace-catalog-lsp.log`,
`slim-workspace-catalog-clippy.log`, `slim-workspace-catalog-gate.log`.

A bateria Luna expandida teve cinco pares completos e um par falho. O par
`20260905-093759Z-json_cli` terminou no primeiro request Slim com
MalformedToolCall (2783 ms de provider, 0 ferramentas executadas, usage desconhecido).
O ledger identifica preparação de list, mas não contém o fragmento bruto rejeitado.
Não há prova suficiente para atribuir essa falha a um campo específico do protocolo.
O Pi desse par passou. Os três cenários restantes foram executados sem substituir
o par falho. A tabela contém somente os pares completos; não é uma bateria 6/6.

#### Luna: cinco pares completos

| Evidência | Tokens Slim/Pi | Calls Slim/Pi |
|---|---:|---:|
| [merge_ranges](20260905-093618Z/summary.json) | 9054 / 11011 | 4 / 5 |
| [repair_catalog](20260905-093712Z-repair_catalog/summary.json) | 10077 / 9893 | 5 / 5 |
| [js_pagination](20260905-094237Z-js_pagination/summary.json) | 10784 / 10561 | 5 / 5 |
| [ledger_audit](20260905-094329Z-ledger_audit/summary.json) | 18457 / 17409 | 6 / 6 |
| [config_migration](20260905-094429Z-config_migration/summary.json) | 25722 / 23175 | 10 / 9 |

Total dos pares completos: 74094 / 72049 tokens, Slim 2.84% acima.

### DeepSeek: thinking ativado e continuação preservada

O adapter Chat ativa thinking quando reasoning é solicitado para DeepSeek V4.
O estado exato retornado em reasoning_content é acumulado separadamente da saída
visível, associado a modelo/credencial/endpoint e reenviado nos próximos requests.
Debug não imprime esse estado; logs visíveis continuam redigidos. Trocar de escopo
não envia o estado ao novo destino. O orçamento inclui esses bytes e a compactação
preserva uma continuação ativa de ferramentas, sem inserir o estado no resumo.

Teste HTTP com duas chamadas reais localhost verificou ativação, fragmentos,
retorno exato após read, final sem tool e isolamento ao trocar modelo, chave ou
endpoint. Inclui uma chave fixture dividida entre fragmentos: o retorno do protocolo
é íntegro e o log visível não a expõe. Gate: 1127 passed / 0 failed / 1 ignored /
73 suítes / 0 compiler warnings. Clippy e refresh-slim.ps1 -Test: exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 06:47:23)
```

15073280 bytes, SHA-256 target/PATH
`2CF32F6631AE51FCB926F779480AD8499D9BD655265CD520FAD0436B2F60E1D0`.
Logs: `%TEMP%/slim-chat-thinking-focused.log`, `slim-chat-thinking-clippy.log`,
`slim-chat-thinking-gate.log`. A bateria seguinte usa esse hash, high e thinking
ativado, e o auditor exige raciocínio efetivo nos eventos Slim. O gateway não
preenche reasoning_tokens no usage Slim, embora emita ReasoningDelta: zero nessa
coluna não significa thinking desligado. Input/output completos são contabilizados.

| Evidência | Tokens Slim/Pi | Calls Slim/Pi |
|---|---:|---:|
| [merge_ranges](20260905-094835Z/summary.json) | 16707 / 15066 | 5 / 5 |
| [repair_catalog](20260905-094914Z-repair_catalog/summary.json) | 15485 / 13014 | 5 / 5 |
| [json_cli](20260905-094939Z-json_cli/summary.json) | 19156 / 33799 | 5 / 7 |
| [js_pagination](20260905-095051Z-js_pagination/summary.json) | 14953 / 13241 | 5 / 5 |
| [ledger_audit](20260905-095120Z-ledger_audit/summary.json) | 38732 / 31107 | 7 / 6 |
| [config_migration](20260905-095251Z-config_migration/summary.json) | 32593 / 15254 | 8 / 5 |

Todos os seis pares passaram nos oráculos, fixtures intactos; daily e auditor,
exit 0. Total 137626 / 121481 tokens: Slim 13,29% acima. A meta ainda não foi
atingida. Os traces mostram custo de numeração em read e mais rodadas de patch
na migração de configurações (8/5 calls). Isso direcionou a próxima etapa para
leitura nativa sem prefixos e substituições em lote no mesmo arquivo.

O runner passou a concluir os cenários planejados mesmo após uma falha individual,
registrando failures e terminando com exit 1 se houver qualquer falha. Nenhum
resultado antigo foi apagado ou substituído.


### Leitura sem prefixos e patches em lote

O read nativo devolve texto sem numeração adicionada e preserva CRLF/EOF. Os
limites, offset, footer de paginação, proteção do workspace e digest de leitura
continuam no mesmo leitor. A API pública de leitura numerada mantém seu contrato.
Os testes HTTP verificam o conteúdo exato e o replay/cache de resultados. A fixture
com sete providers/dez variantes compara payloads antes/depois, alterando somente
as representações dos resultados de list/search/read e preservando os demais campos.

O patch nativo anuncia `path, edits:[{expected,replacement}]`, de 1 a 64 entradas.
As substituições são ordenadas: cada expected deve ocorrer uma vez no conteúdo
resultante das anteriores. Tudo é calculado sob a guarda existente antes da única
substituição do arquivo. Falha tardia, ambiguidade ou expansão além de 10 MiB deixam
o arquivo original intacto. O receipt registra a dependência inicial e o estado
final; cache e notificação LSP são atualizados uma vez após sucesso. A chamada
antiga com expected/replacement continua aceita, sem mistura das duas formas.

Os testes novos cobrem lote ordenado, falha na segunda edição, ambiguidade,
entradas inválidas, CRLF, limite após edição anterior e invalidação de read em cache.
Os asserts da leitura nativa foram atualizados para exigir o conteúdo bruto completo,
incluindo CRLF e EOF; os testes da API pública numerada permanecem. Um primeiro gate
parou num assert antigo da CLI que esperava o prefixo `1:`; o assert foi corrigido
para o novo conteúdo, sem relaxar a validação do texto retornado.


Gate completo do conjunto acima: **1129 passed / 0 failed / 1 ignored / 73 suítes /
0 compiler warnings**, Clippy --workspace --all-targets -- -D warnings exit 0,
refresh-slim.ps1 -Test exit 0. Nenhum novo framework ou dependência.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 07:08:25)
```

Target/PATH: **15084544 bytes**, SHA-256 idêntico
`B3CE3A4D0F95807AD5DEF509207EFE8E412971B1066D349A0505F0DC36C8F9CC`.
`Slim --version`: slim 0.1.0, exit 0. Logs: `%TEMP%/slim-batch-patch-focused.log`,
`slim-batch-patch-clippy.log`, `slim-batch-patch-gate.log`.

A bateria seguinte foi fixada em seis cenários × duas rodadas × dois modelos,
24 pares. Os grupos Luna e DeepSeek executam em paralelo entre si, em workspaces
independentes; os braços de cada par permanecem sequenciais e alternam ordem.
O objetivo é tokens por tarefa com qualidade validada, não comparar a latência
com campanhas antigas serializadas. O sufixo opencode-go no nome do diretório
evita colisão de timestamps entre os grupos; manifests identificam a rota/modelo.


### Bateria de 24 pares: leitura bruta e patch em lote

Binário B3CE3A4D0F95807AD5DEF509207EFE8E412971B1066D349A0505F0DC36C8F9CC. Seis cenários, duas rodadas por modelo, todos planejados antes da execução. Tokens incluem entrada em cache e saída.

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| luna | 1 | merge_ranges | 8835 | 8358 | 4/4 | [20260905-100929Z](20260905-100929Z/summary.json) |
| luna | 1 | repair_catalog | 9774 | 9866 | 5/5 | [20260905-101021Z-repair_catalog](20260905-101021Z-repair_catalog/summary.json) |
| luna | 1 | json_cli | 17609 | 14214 | 6/5 | [20260905-101110Z-json_cli](20260905-101110Z-json_cli/summary.json) |
| luna | 1 | js_pagination | 10075 | 12662 | 5/6 | [20260905-101304Z-js_pagination](20260905-101304Z-js_pagination/summary.json) |
| luna | 1 | ledger_audit | 24662 | 18242 | 7/6 | [20260905-101403Z-ledger_audit](20260905-101403Z-ledger_audit/summary.json) |
| luna | 1 | config_migration | 12227 | 10733 | 6/5 | [20260905-101518Z-config_migration](20260905-101518Z-config_migration/summary.json) |
| luna | 2 | merge_ranges | 9067 | 8743 | 4/4 | [20260905-101610Z](20260905-101610Z/summary.json) |
| luna | 2 | repair_catalog | 9295 | 9808 | 5/5 | [20260905-101701Z-repair_catalog](20260905-101701Z-repair_catalog/summary.json) |
| luna | 2 | json_cli | 12696 | 14573 | 5/5 | [20260905-101745Z-json_cli](20260905-101745Z-json_cli/summary.json) |
| luna | 2 | js_pagination | 10132 | 10766 | 5/5 | [20260905-101924Z-js_pagination](20260905-101924Z-js_pagination/summary.json) |
| luna | 2 | ledger_audit | 17205 | 17895 | 6/6 | [20260905-102017Z-ledger_audit](20260905-102017Z-ledger_audit/summary.json) |
| luna | 2 | config_migration | 27221 | 19478 | 10/9 | [20260905-102116Z-config_migration](20260905-102116Z-config_migration/summary.json) |

luna: 168798 / 155338 tokens (+8.66% Slim); 6/12 vitórias em tokens. Todos os 12 pares passaram, fixtures intactos, uso completo, auditor e daily exit 0.

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| deepseek | 1 | merge_ranges | 16784 | 24840 | 5/6 | [20260905-100929Z-opencode-go](20260905-100929Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 14581 | 15215 | 5/5 | [20260905-101025Z-opencode-go-repair_catalog](20260905-101025Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 1 | json_cli | 24608 | 27220 | 6/7 | [20260905-101051Z-opencode-go-json_cli](20260905-101051Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | js_pagination | 14536 | 16219 | 5/5 | [20260905-101159Z-opencode-go-js_pagination](20260905-101159Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | ledger_audit | 32684 | 24853 | 6/6 | [20260905-101227Z-opencode-go-ledger_audit](20260905-101227Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 1 | config_migration | 18717 | 14557 | 6/5 | [20260905-101344Z-opencode-go-config_migration](20260905-101344Z-opencode-go-config_migration/summary.json) |
| deepseek | 2 | merge_ranges | 18935 | 22351 | 5/6 | [20260905-101413Z-opencode-go](20260905-101413Z-opencode-go/summary.json) |
| deepseek | 2 | repair_catalog | 15256 | 14684 | 5/5 | [20260905-101510Z-opencode-go-repair_catalog](20260905-101510Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | json_cli | 25418 | 17217 | 6/6 | [20260905-101541Z-opencode-go-json_cli](20260905-101541Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | js_pagination | 16260 | 15754 | 5/5 | [20260905-101637Z-opencode-go-js_pagination](20260905-101637Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | ledger_audit | 25343 | 36501 | 6/7 | [20260905-101703Z-opencode-go-ledger_audit](20260905-101703Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 2 | config_migration | 17859 | 22388 | 6/7 | [20260905-101759Z-opencode-go-config_migration](20260905-101759Z-opencode-go-config_migration/summary.json) |

deepseek: 240981 / 251799 tokens (-4.30% Slim); 7/12 vitórias em tokens. Todos os 12 pares passaram, fixtures intactos, uso completo, auditor e daily exit 0.


A meta ampla ainda não foi atingida: vantagem no DeepSeek, desvantagem no Luna. Não há prova de ganho universal ou de equivalência fora dos oráculos exercitados. Os traces ainda mostram paginação padrão após 80 linhas e cálculo manual de dados: em 20260905-101227Z-opencode-go-ledger_audit o Slim leu ledger.csv em duas chamadas e transcreveu os totais, enquanto o Pi usou script. Em 20260905-101403Z-ledger_audit houve gravação de uma sequência literal de newline e correção posterior. A próxima mudança ataca essas operações gerais; não altera cenários, esforço nem oráculos.


### Leitura padrão de 200 linhas e cálculo executável

O padrão de `read` passou de 80 para 200 linhas. `offset` e `max_lines` explícitos,
o cap de 4096 linhas e os limites em bytes continuam aplicados. Um teste novo
cobre EOF na linha 200, continuação na linha 201 e leitura explícita de um trecho.
O teste de identidade canônica do governor foi ajustado para usar a constante
padrão; a primeira execução do gate revelou a expectativa antiga de 80 linhas.

O prompt compartilhado v1.5 orienta cálculos e transformações repetitivas por
scripts, com gravação direta dos resultados calculados. A descrição de shell
passa a distinguir edição de texto de processamento de dados. Não há condição
por modelo, linguagem ou cenário; esforço de raciocínio e obrigação de validar
continuam iguais. Isso pretende evitar cálculo manual, transcrição e chamadas
de correção, mas a redução total depende da medição real seguinte.

A próxima bateria mantém os mesmos seis cenários e duas rodadas por modelo,
24 pares, ordem alternada e oráculos externos inalterados. Nenhum par anterior
será removido ou usado como substituto de uma falha da nova campanha.

Gate: 1130 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings.
`refresh-slim.ps1 -Test` exit 0. Target/PATH com 15084544 bytes e SHA-256 idêntico
`37497B720F8DCFEC9211F45EA5DD7B5254D159E544B1B0D31EA31D1A85082F0A`.
`Slim --version`: slim 0.1.0, exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 07:31:33)
```

Logs de gate: `%TEMP%/slim-executable-work-focused.log` e
`slim-executable-work-gate.log`. Campanhas: `slim-economy-executable-luna.log`
e `slim-economy-executable-deepseek.log` no mesmo diretório temporário.

Clippy `--workspace --all-targets -- -D warnings`: exit 0; log `%TEMP%/slim-executable-work-clippy.log`.


#### Resultado dos 24 pares com read 200 e prompt v1.5


| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| luna | 1 | merge_ranges | 9046 | 10307 | 4/5 | [20260905-103207Z](20260905-103207Z/summary.json) |
| luna | 1 | repair_catalog | 10048 | 9759 | 5/5 | [20260905-103310Z-repair_catalog](20260905-103310Z-repair_catalog/summary.json) |
| luna | 1 | json_cli | 15095 | 21391 | 5/7 | [20260905-103358Z-json_cli](20260905-103358Z-json_cli/summary.json) |
| luna | 1 | js_pagination | 10449 | 10387 | 5/5 | [20260905-103556Z-js_pagination](20260905-103556Z-js_pagination/summary.json) |
| luna | 1 | ledger_audit | 18694 | 17655 | 6/6 | [20260905-103650Z-ledger_audit](20260905-103650Z-ledger_audit/summary.json) |
| luna | 1 | config_migration | 34510 | 21394 | 11/8 | [20260905-103749Z-config_migration](20260905-103749Z-config_migration/summary.json) |
| luna | 2 | merge_ranges | 10578 | 10500 | 5/5 | [20260905-103918Z](20260905-103918Z/summary.json) |
| luna | 2 | repair_catalog | 9852 | 11055 | 5/6 | [20260905-104023Z-repair_catalog](20260905-104023Z-repair_catalog/summary.json) |
| luna | 2 | json_cli | 19572 | 23402 | 6/7 | [20260905-104114Z-json_cli](20260905-104114Z-json_cli/summary.json) |
| luna | 2 | js_pagination | 10335 | 11009 | 5/5 | [20260905-104335Z-js_pagination](20260905-104335Z-js_pagination/summary.json) |
| luna | 2 | ledger_audit | 13486 | 17832 | 5/6 | [20260905-104429Z-ledger_audit](20260905-104429Z-ledger_audit/summary.json) |
| luna | 2 | config_migration | 23711 | 13034 | 9/6 | [20260905-104534Z-config_migration](20260905-104534Z-config_migration/summary.json) |

luna: 185376 / 177725 tokens (+4.30% Slim); 6/12 vitórias. Todos os 12 pares passaram, fixtures intactos, uso completo, auditor e daily exit 0.


| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| deepseek | 1 | merge_ranges | 19882 | 20101 | 5/6 | [20260905-103217Z-opencode-go](20260905-103217Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 15283 | 14920 | 5/5 | [20260905-103318Z-opencode-go-repair_catalog](20260905-103318Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 1 | json_cli | 21129 | 50618 | 6/8 | [20260905-103400Z-opencode-go-json_cli](20260905-103400Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | js_pagination | 14616 | 15233 | 5/5 | [20260905-103527Z-opencode-go-js_pagination](20260905-103527Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | ledger_audit | 37879 | 34763 | 7/7 | [20260905-103604Z-opencode-go-ledger_audit](20260905-103604Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 1 | config_migration | 25311 | 65577 | 7/11 | [20260905-103721Z-opencode-go-config_migration](20260905-103721Z-opencode-go-config_migration/summary.json) |
| deepseek | 2 | merge_ranges | 15956 | 22110 | 5/6 | [20260905-103833Z-opencode-go](20260905-103833Z-opencode-go/summary.json) |
| deepseek | 2 | repair_catalog | 15397 | 14573 | 5/5 | [20260905-103925Z-opencode-go-repair_catalog](20260905-103925Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | json_cli | 29584 | 37804 | 7/7 | [20260905-104003Z-opencode-go-json_cli](20260905-104003Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | js_pagination | 16292 | 16109 | 5/5 | [20260905-104146Z-opencode-go-js_pagination](20260905-104146Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | ledger_audit | 20859 | 25824 | 5/6 | [20260905-104221Z-opencode-go-ledger_audit](20260905-104221Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 2 | config_migration | 19540 | 19139 | 6/6 | [20260905-104307Z-opencode-go-config_migration](20260905-104307Z-opencode-go-config_migration/summary.json) |

deepseek: 251728 / 336771 tokens (-25.25% Slim); 7/12 vitórias. Todos os 12 pares passaram, fixtures intactos, uso completo, auditor e daily exit 0.


A meta ampla continua não comprovada. Há ganho agregado no DeepSeek e desvantagem no Luna. Resultados de outros binários não foram somados a esta bateria.

### Patch: inserção de linhas sem criar finais misturados

O trace `20260905-103749Z-config_migration` demonstrou duas rejeições de patch após inserções de múltiplas linhas em trechos de uma linha. Arquivos originalmente CRLF terminaram com 20/11 CRLF e três LF isolados cada. A falta inicial de vírgulas foi erro do modelo; a mistura de finais criada pelo patch agravou a recuperação e não explica sozinha todo o custo da tarefa.

O código só normalizava replacement quando precisava converter expected de LF para CRLF. Um expected sem newline casava diretamente e inseria replacement LF literal. Agora, quando há uma ocorrência exata de um trecho sem newline em arquivo CRLF uniforme, novas linhas exclusivamente LF herdam CRLF. Conteúdo, unicidade e comportamento em arquivo já misturado continuam preservados.

Regressão RED: `first\r\none\ntwo\r\nlast\r\n` quando se esperava CRLF uniforme. GREEN nos 10 testes de native_tool_recovery e 25 tool_contracts, incluindo nova aplicação de trecho LF após a inserção e rejeição sem mutação quando expansão de newlines ultrapassa 10 MiB. Clippy --workspace --all-targets -- -D warnings exit 0. Logs `%TEMP%/slim-patch-crlf-red.log`, `slim-patch-crlf-green.log`, `slim-patch-crlf-clippy.log`.

Gate completo: 1131 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings;
Clippy --workspace --all-targets -- -D warnings exit 0; refresh-slim.ps1 -Test exit 0.
Target/PATH: 15085568 bytes, SHA-256 idêntico
`2BBC54B0321065E7CCD303B62FD5922D0934DA8123728F0A5691AC414F4FAEB2`.
`Slim --version`: slim 0.1.0, exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 07:52:48)
```

Nova bateria fixada: seis cenários, duas rodadas, dois modelos (24 pares).
Até dois cenários simultâneos por modelo; cada cenário mantém braços sequenciais
e ordem alternada. Mesmo esforço high, rotas, prompts e oráculos. A concorrência
visa reduzir tempo de coleta: não comparar latência com baterias anteriores.
Logs `%TEMP%/slim-economy-crlf-{luna,deepseek}-{scenario}.log`, um por cenário.
Código do binário fixo durante todos os pares; nenhum resultado anterior substitui
falhas ou lacunas desta bateria. Gate: `%TEMP%/slim-patch-crlf-gate.log`.


#### Resultado da bateria com patch CRLF corrigido


| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| luna | 1 | config_migration | 12974 | 27094 | 6/9 | [20260905-105833Z-config_migration](20260905-105833Z-config_migration/summary.json) |
| luna | 2 | config_migration | 12057 | 13178 | 6/6 | [20260905-105943Z-config_migration](20260905-105943Z-config_migration/summary.json) |
| luna | 1 | js_pagination | 10424 | 10706 | 5/5 | [20260905-105506Z-js_pagination](20260905-105506Z-js_pagination/summary.json) |
| luna | 2 | js_pagination | 10506 | 10215 | 5/5 | [20260905-105555Z-js_pagination](20260905-105555Z-js_pagination/summary.json) |
| luna | 1 | json_cli | 15362 | 16424 | 6/6 | [20260905-105502Z-json_cli](20260905-105502Z-json_cli/summary.json) |
| luna | 2 | json_cli | 15620 | 13157 | 5/5 | [20260905-105646Z-json_cli](20260905-105646Z-json_cli/summary.json) |
| luna | 1 | ledger_audit | 22637 | 14109 | 7/5 | [20260905-105651Z-ledger_audit](20260905-105651Z-ledger_audit/summary.json) |
| luna | 2 | ledger_audit | 16962 | 17628 | 6/6 | [20260905-105800Z-ledger_audit](20260905-105800Z-ledger_audit/summary.json) |
| luna | 1 | merge_ranges | 8603 | 8762 | 4/4 | [20260905-105319Z](20260905-105319Z/summary.json) |
| luna | 2 | merge_ranges | 9170 | 10759 | 4/5 | [20260905-105407Z](20260905-105407Z/summary.json) |
| luna | 1 | repair_catalog | 9832 | 9812 | 5/5 | [20260905-105319Z-repair_catalog](20260905-105319Z-repair_catalog/summary.json) |
| luna | 2 | repair_catalog | 9351 | 10884 | 5/6 | [20260905-105408Z-repair_catalog](20260905-105408Z-repair_catalog/summary.json) |

luna: 12 pares completos, 153498 / 162728 tokens (-5.67% Slim); 8/12 vitórias. Estes pares passaram nos oráculos, fixtures intactos e uso completo.


| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| deepseek | 2 | config_migration | 19011 | 16717 | 6/5 | [20260905-105919Z-opencode-go-config_migration](20260905-105919Z-opencode-go-config_migration/summary.json) |
| deepseek | 1 | js_pagination | 24041 | 13789 | 7/5 | [20260905-105456Z-opencode-go-js_pagination](20260905-105456Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | js_pagination | 16619 | 14751 | 5/5 | [20260905-105528Z-opencode-go-js_pagination](20260905-105528Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | json_cli | 34991 | 35589 | 7/8 | [20260905-105434Z-opencode-go-json_cli](20260905-105434Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | json_cli | 20349 | 26997 | 6/7 | [20260905-105611Z-opencode-go-json_cli](20260905-105611Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | ledger_audit | 25431 | 32511 | 6/6 | [20260905-105558Z-opencode-go-ledger_audit](20260905-105558Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 2 | ledger_audit | 24335 | 31960 | 5/6 | [20260905-105718Z-opencode-go-ledger_audit](20260905-105718Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 1 | merge_ranges | 14775 | 15291 | 4/5 | [20260905-105319Z-opencode-go](20260905-105319Z-opencode-go/summary.json) |
| deepseek | 2 | merge_ranges | 17595 | 23900 | 5/6 | [20260905-105403Z-opencode-go](20260905-105403Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 13984 | 14393 | 5/5 | [20260905-105319Z-opencode-go-repair_catalog](20260905-105319Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | repair_catalog | 15465 | 14864 | 5/5 | [20260905-105344Z-opencode-go-repair_catalog](20260905-105344Z-opencode-go-repair_catalog/summary.json) |

deepseek: 11 pares completos, 226596 / 240762 tokens (-5.88% Slim); 7/11 vitórias. Estes pares passaram nos oráculos, fixtures intactos e uso completo.


**Falha planejada e preservada:** DeepSeek, config_migration, rodada 1: [20260905-105700Z-opencode-go-config_migration](20260905-105700Z-opencode-go-config_migration/slim.stdout.jsonl). Slim exit 21, oráculo exit 1; Pi passou. Terceira requisição recebeu cabeçalhos/primeiro byte aos 869 ms, nenhum evento semântico, e falhou em transporte após 120009 ms. Só quatro ferramentas de leitura/listagem tinham rodado. Uso da requisição é desconhecido; não foi contado como zero nem substituído pelo segundo round. A origem do timeout (gateway/rede) não foi isolada. Daily DeepSeek exit 1; Luna exit 0.

A economia é agregada, não ampla por cenário: Luna ainda perde em JSON CLI e ledger; DeepSeek perde em JavaScript e alguns pares de reparo/migração. Não é válido concluir superioridade universal ou 24/24 de qualidade. O caminho de patch foi corrigido com RED/GREEN; o comportamento do modelo ainda cria chamadas evitáveis para executar scripts.

### Execução direta com argumentos estruturados

O tool shell ganha `args` opcional. Com array, command é o executável e cada argumento segue literalmente pelo ProcessRunner existente; sem args ou com null, permanece o script PowerShell. Não há novo interpretador, dependência ou escolha por modelo. Executáveis com caminho relativo são resolvidos a partir do workspace; nomes simples usam o resolver/PATH existente. Timeout, cancelamento, captura limitada, progresso e código de saída usam a mesma implementação.

O resumo da ferramenta mostra programa/argumentos; a classificação causal reconhece cargo check/test/clippy diretos e mantém --fix como operação potencialmente mutável. Testes cobrem argumentos literais (espaços/aspas/Unicode/símbolos), saída 7, modo PowerShell, entrada inválida sem efeito e executável ausente sem fallback. A intenção é evitar camadas de quoting e arquivos auxiliares só para executar um script; efeito em tokens ainda não medido.

Gate completo: 1132 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings;
Clippy --workspace --all-targets -- -D warnings exit 0; refresh-slim.ps1 -Test exit 0.
Target/PATH: 15096832 bytes, SHA-256 idêntico
`D85CE4DFE51FDED4330408AE6865DDAB0B8AE5833D485682D5CF6FAFD71F5879`.
`Slim --version`: slim 0.1.0, exit 0.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 08:21:37)
```

Logs `%TEMP%/slim-direct-args-focused.log`, `slim-direct-args-clippy.log`,
`slim-direct-args-gate.log`. Bateria fixada novamente em 24 pares: seis cenários,
duas rodadas, dois modelos, até dois cenários simultâneos por modelo, braços
sequenciais/alternados dentro de cada par. A ordem de cenários começou por
ledger_audit/json_cli, depois merge_ranges, repair_catalog, js_pagination e
config_migration; nenhum cenário foi removido. Esforço high, prompts e oráculos
inalterados. Logs `%TEMP%/slim-economy-direct-{luna,deepseek}-{scenario}.log`.


#### Resultado da bateria com argumentos diretos

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| luna | 1 | config_migration | 12598 | 11896 | 6/6 | [20260905-112702Z-config_migration](20260905-112702Z-config_migration/summary.json) |
| luna | 2 | config_migration | 27581 | 12414 | 10/5 | [20260905-112801Z-config_migration](20260905-112801Z-config_migration/summary.json) |
| luna | 1 | js_pagination | 10225 | 10471 | 5/5 | [20260905-112547Z-js_pagination](20260905-112547Z-js_pagination/summary.json) |
| luna | 2 | js_pagination | 10193 | 10936 | 5/5 | [20260905-112637Z-js_pagination](20260905-112637Z-js_pagination/summary.json) |
| luna | 1 | json_cli | 14795 | 16579 | 5/6 | [20260905-112156Z-json_cli](20260905-112156Z-json_cli/summary.json) |
| luna | 2 | json_cli | 14310 | 14644 | 5/5 | [20260905-112340Z-json_cli](20260905-112340Z-json_cli/summary.json) |
| luna | 1 | ledger_audit | 14357 | 10231 | 5/4 | [20260905-112156Z-ledger_audit](20260905-112156Z-ledger_audit/summary.json) |
| luna | 2 | ledger_audit | 21481 | 17480 | 7/6 | [20260905-112246Z-ledger_audit](20260905-112246Z-ledger_audit/summary.json) |
| luna | 1 | merge_ranges | 8677 | 8147 | 4/4 | [20260905-112356Z](20260905-112356Z/summary.json) |
| luna | 2 | merge_ranges | 9324 | 10400 | 4/5 | [20260905-112444Z](20260905-112444Z/summary.json) |
| luna | 1 | repair_catalog | 9344 | 9687 | 5/5 | [20260905-112533Z-repair_catalog](20260905-112533Z-repair_catalog/summary.json) |

luna: 11 pares completos; 152885 / 132885 tokens (+15.05% Slim), 6/11 vitórias. Os pares tabulados passaram nos oráculos, com fixtures preservados e uso completo.

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| deepseek | 1 | config_migration | 19863 | 62385 | 6/12 | [20260905-112536Z-opencode-go-config_migration](20260905-112536Z-opencode-go-config_migration/summary.json) |
| deepseek | 2 | config_migration | 19175 | 14362 | 6/5 | [20260905-112635Z-opencode-go-config_migration](20260905-112635Z-opencode-go-config_migration/summary.json) |
| deepseek | 1 | js_pagination | 14994 | 15052 | 5/5 | [20260905-112529Z-opencode-go-js_pagination](20260905-112529Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | js_pagination | 16799 | 15873 | 5/5 | [20260905-112552Z-opencode-go-js_pagination](20260905-112552Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | json_cli | 19260 | 25723 | 6/6 | [20260905-112156Z-opencode-go-json_cli](20260905-112156Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | json_cli | 24227 | 15862 | 6/5 | [20260905-112249Z-opencode-go-json_cli](20260905-112249Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | ledger_audit | 25403 | 38231 | 5/7 | [20260905-112156Z-opencode-go-ledger_audit](20260905-112156Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 2 | ledger_audit | 30112 | 24198 | 6/6 | [20260905-112337Z-opencode-go-ledger_audit](20260905-112337Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 1 | merge_ranges | 16127 | 16300 | 5/5 | [20260905-112419Z-opencode-go](20260905-112419Z-opencode-go/summary.json) |
| deepseek | 2 | merge_ranges | 18757 | 15137 | 5/5 | [20260905-112454Z-opencode-go](20260905-112454Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 15310 | 13471 | 5/5 | [20260905-112429Z-opencode-go-repair_catalog](20260905-112429Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | repair_catalog | 15587 | 14393 | 5/5 | [20260905-112453Z-opencode-go-repair_catalog](20260905-112453Z-opencode-go-repair_catalog/summary.json) |

deepseek: 12 pares completos; 235614 / 270987 tokens (-13.05% Slim), 5/12 vitórias. Os pares tabulados passaram nos oráculos, com fixtures preservados e uso completo.

**Falha preservada:** Luna, repair_catalog, rodada 2: [20260905-112630Z-repair_catalog](20260905-112630Z-repair_catalog/slim.stdout.jsonl). Slim exit 21/oráculo 1; Pi passou. A primeira resposta terminou em MalformedToolCall antes de qualquer ferramenta; uso desconhecido, não contado como zero nem substituído. Daily Luna exit 1; DeepSeek exit 0.

Não houve economia ampla: Luna consumiu mais nos pares completos; o ganho agregado DeepSeek depende principalmente da primeira migração cara do Pi (62385 tokens). Args foi utilizado em execuções reais e sua passagem literal funciona, mas esta bateria não demonstra redução consistente de chamadas ou tokens atribuível ao recurso.

### Identidade na conclusão de chamadas Responses e prefixo v1.6

A falha Luna foi reproduzida na tentativa 13 de uma captura limitada ao primeiro turno do adapter/runtime nativos. Evidência primária: [eventos brutos capturados](20260905-114011Z-protocol-capture/13.events.jsonl), com encrypted_content omitido; nenhum cabeçalho ou credencial foi registrado. O código temporário de captura foi removido após o diagnóstico.

O stream contém list, read e outro list com argumentos idênticos ao primeiro, mas call_id e output_index distintos. O adapter preservava identidade nos deltas, porém emitia conclusão legada apenas com nome/argumentos; attach_legacy_call encontrava duas correspondências e rejeitava uma resposta válida. ToolCallComplete agora preserva index/id até a associação com o buffer correspondente. Conclusão sem deltas continua suportada. Identidade conflitante, JSON inválido e argumentos divergentes continuam rejeitados antes de publicar ferramentas.

RED: teste HTTP com a sequência reduzida falhou com MalformedToolCall. GREEN: a mesma sequência publica cada identidade exatamente uma vez, incluindo conclusão sem deltas; 53 agent_loop + 34 provider_adapters + 59 provider_http passaram. Teste unitário adicional rejeitou cinco conflitos de conclusão sem publicar ferramentas. Logs %TEMP%/slim-codex-identity-{red,green,negative}.log. Estes testes são locais, não novas execuções dos pares que falharam.

O prefixo v1.6 também reduz prosa redundante do prompt e das descrições compartilhadas, mantendo os parâmetros, limites, guardas de escrita, permissões e obrigações de qualidade. A redução de bytes é estrutural; o efeito em tokens/qualidade depende da nova medição, sem atribuir-lhe os resultados da bateria anterior.


Bateria prospectiva v1.6: **48 pares planejados**, quatro rodadas dos seis cenários,
Luna/openai-codex e DeepSeek v4 Flash/opencode-go, esforço high. Mesmos prompts,
rotas, verificações externas e preservação dos fixtures; ordem dos braços alternada.
Até dois cenários simultâneos por modelo. Nenhum ajuste de código durante a coleta,
nenhuma substituição de falha. Reportar total, mediana das razões por par e agregados
por cenário para expor ganhos concentrados em outliers. Logs
`%TEMP%/slim-economy-compact-{luna,deepseek}-{scenario}.log`.


Gate v1.6 concluído: 1134 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler
warnings; Clippy --workspace --all-targets -- -D warnings exit 0. O primeiro gate
foi interrompido pela expectativa de marcador Unicode no teste do prompt; os
marcadores foram preservados e o gate completo seguinte passou. Nenhum teste foi
removido ou enfraquecido. Logs `%TEMP%/slim-compact-identity-{gate,clippy}.log`.
Target/PATH idênticos: 15096320 bytes, SHA-256
`8FB0E5F59FFA668033B5439C20968AEEB73D3F2A53191C978E2D467FBAE7BFA0`.
`Slim --version`: slim 0.1.0, exit 0. Prompt raw UTF-8: 1911 → 1594 bytes (-16,59%).

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 09:03:43)
```


#### Resultado completo da bateria v1.6

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| luna | 1 | config_migration | 33318 | 13042 | 12/6 | [20260905-121619Z-config_migration](20260905-121619Z-config_migration/summary.json) |
| luna | 2 | config_migration | 23205 | 13884 | 9/6 | [20260905-121737Z-config_migration](20260905-121737Z-config_migration/summary.json) |
| luna | 3 | config_migration | 13410 | 12838 | 7/6 | [20260905-121854Z-config_migration](20260905-121854Z-config_migration/summary.json) |
| luna | 4 | config_migration | 32880 | 13744 | 11/6 | [20260905-121956Z-config_migration](20260905-121956Z-config_migration/summary.json) |
| luna | 1 | js_pagination | 9724 | 11054 | 5/5 | [20260905-121237Z-js_pagination](20260905-121237Z-js_pagination/summary.json) |
| luna | 2 | js_pagination | 9762 | 10332 | 5/5 | [20260905-121339Z-js_pagination](20260905-121339Z-js_pagination/summary.json) |
| luna | 3 | js_pagination | 9447 | 10794 | 5/5 | [20260905-121428Z-js_pagination](20260905-121428Z-js_pagination/summary.json) |
| luna | 4 | js_pagination | 10002 | 12302 | 5/6 | [20260905-121524Z-js_pagination](20260905-121524Z-js_pagination/summary.json) |
| luna | 1 | json_cli | 33395 | 29654 | 9/8 | [20260905-120408Z-json_cli](20260905-120408Z-json_cli/summary.json) |
| luna | 2 | json_cli | 14417 | 15021 | 5/5 | [20260905-120632Z-json_cli](20260905-120632Z-json_cli/summary.json) |
| luna | 3 | json_cli | 22733 | 16586 | 8/6 | [20260905-120833Z-json_cli](20260905-120833Z-json_cli/summary.json) |
| luna | 4 | json_cli | 16156 | 13793 | 6/5 | [20260905-121045Z-json_cli](20260905-121045Z-json_cli/summary.json) |
| luna | 1 | ledger_audit | 22466 | 30620 | 7/7 | [20260905-120408Z-ledger_audit](20260905-120408Z-ledger_audit/summary.json) |
| luna | 2 | ledger_audit | 16272 | 18021 | 6/6 | [20260905-120526Z-ledger_audit](20260905-120526Z-ledger_audit/summary.json) |
| luna | 3 | ledger_audit | 16283 | 26713 | 6/8 | [20260905-120629Z-ledger_audit](20260905-120629Z-ledger_audit/summary.json) |
| luna | 4 | ledger_audit | 17021 | 10929 | 6/5 | [20260905-120739Z-ledger_audit](20260905-120739Z-ledger_audit/summary.json) |
| luna | 1 | merge_ranges | 8571 | 11694 | 4/5 | [20260905-120836Z](20260905-120836Z/summary.json) |
| luna | 2 | merge_ranges | 8264 | 9186 | 4/4 | [20260905-120937Z](20260905-120937Z/summary.json) |
| luna | 3 | merge_ranges | 8255 | 10697 | 4/5 | [20260905-121032Z](20260905-121032Z/summary.json) |
| luna | 4 | merge_ranges | 8326 | 10251 | 4/5 | [20260905-121131Z](20260905-121131Z/summary.json) |
| luna | 1 | repair_catalog | 8626 | 9812 | 5/5 | [20260905-121231Z-repair_catalog](20260905-121231Z-repair_catalog/summary.json) |
| luna | 2 | repair_catalog | 9372 | 12065 | 5/7 | [20260905-121327Z-repair_catalog](20260905-121327Z-repair_catalog/summary.json) |
| luna | 3 | repair_catalog | 9098 | 11397 | 5/7 | [20260905-121424Z-repair_catalog](20260905-121424Z-repair_catalog/summary.json) |
| luna | 4 | repair_catalog | 11698 | 11292 | 6/7 | [20260905-121521Z-repair_catalog](20260905-121521Z-repair_catalog/summary.json) |

luna: 24/24 pares completos; 372701 / 345721 tokens (+7.80% Slim); mediana das razões por par -9.87%; 15/24 vitórias. Todos os pares tabulados passaram nos oráculos, com fixtures preservados e uso completo.

| Cenário | Pares completos | Slim | Pi | Variação Slim |
|---|---:|---:|---:|---:|
| config_migration | 4 | 102813 | 53508 | +92.15% |
| js_pagination | 4 | 38935 | 44482 | -12.47% |
| json_cli | 4 | 86701 | 75054 | +15.52% |
| ledger_audit | 4 | 72042 | 86283 | -16.50% |
| merge_ranges | 4 | 33416 | 41828 | -20.11% |
| repair_catalog | 4 | 38794 | 44566 | -12.95% |
| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| deepseek | 1 | config_migration | 22521 | 15839 | 7/5 | [20260905-121245Z-opencode-go-config_migration](20260905-121245Z-opencode-go-config_migration/summary.json) |
| deepseek | 2 | config_migration | 18590 | 17769 | 6/6 | [20260905-121405Z-opencode-go-config_migration](20260905-121405Z-opencode-go-config_migration/summary.json) |
| deepseek | 3 | config_migration | 18159 | 17284 | 6/6 | [20260905-121436Z-opencode-go-config_migration](20260905-121436Z-opencode-go-config_migration/summary.json) |
| deepseek | 4 | config_migration | 18129 | 25741 | 6/7 | [20260905-121508Z-opencode-go-config_migration](20260905-121508Z-opencode-go-config_migration/summary.json) |
| deepseek | 1 | js_pagination | 15506 | 16609 | 5/5 | [20260905-121158Z-opencode-go-js_pagination](20260905-121158Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | js_pagination | 15814 | 14146 | 5/5 | [20260905-121242Z-opencode-go-js_pagination](20260905-121242Z-opencode-go-js_pagination/summary.json) |
| deepseek | 3 | js_pagination | 14773 | 14833 | 5/5 | [20260905-121412Z-opencode-go-js_pagination](20260905-121412Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | json_cli | 20435 | 15416 | 5/5 | [20260905-120418Z-opencode-go-json_cli](20260905-120418Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | json_cli | 21913 | 25998 | 5/6 | [20260905-120511Z-opencode-go-json_cli](20260905-120511Z-opencode-go-json_cli/summary.json) |
| deepseek | 3 | json_cli | 21180 | 55088 | 6/9 | [20260905-120633Z-opencode-go-json_cli](20260905-120633Z-opencode-go-json_cli/summary.json) |
| deepseek | 4 | json_cli | 24507 | 34648 | 6/7 | [20260905-120755Z-opencode-go-json_cli](20260905-120755Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | ledger_audit | 25810 | 22350 | 6/6 | [20260905-120418Z-opencode-go-ledger_audit](20260905-120418Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 2 | ledger_audit | 26508 | 37146 | 6/7 | [20260905-120503Z-opencode-go-ledger_audit](20260905-120503Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 3 | ledger_audit | 37114 | 35434 | 7/6 | [20260905-120603Z-opencode-go-ledger_audit](20260905-120603Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 4 | ledger_audit | 76628 | 35947 | 8/7 | [20260905-120719Z-opencode-go-ledger_audit](20260905-120719Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 1 | merge_ranges | 15344 | 15522 | 4/5 | [20260905-120920Z-opencode-go](20260905-120920Z-opencode-go/summary.json) |
| deepseek | 2 | merge_ranges | 16886 | 17482 | 5/5 | [20260905-121006Z-opencode-go](20260905-121006Z-opencode-go/summary.json) |
| deepseek | 3 | merge_ranges | 18548 | 18769 | 5/5 | [20260905-121051Z-opencode-go](20260905-121051Z-opencode-go/summary.json) |
| deepseek | 4 | merge_ranges | 18016 | 17693 | 5/6 | [20260905-121145Z-opencode-go](20260905-121145Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 14708 | 14687 | 5/5 | [20260905-120930Z-opencode-go-repair_catalog](20260905-120930Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | repair_catalog | 14508 | 13982 | 5/5 | [20260905-121003Z-opencode-go-repair_catalog](20260905-121003Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 3 | repair_catalog | 19788 | 15538 | 6/5 | [20260905-121034Z-opencode-go-repair_catalog](20260905-121034Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 4 | repair_catalog | 14333 | 15741 | 5/5 | [20260905-121118Z-opencode-go-repair_catalog](20260905-121118Z-opencode-go-repair_catalog/summary.json) |

deepseek: 23/24 pares completos; 509718 / 513662 tokens (-0.77% Slim); mediana das razões por par +0.14%; 11/23 vitórias. Todos os pares tabulados passaram nos oráculos, com fixtures preservados e uso completo.

| Cenário | Pares completos | Slim | Pi | Variação Slim |
|---|---:|---:|---:|---:|
| config_migration | 4 | 77399 | 76633 | +1.00% |
| js_pagination | 3 | 46093 | 45588 | +1.11% |
| json_cli | 4 | 88035 | 131150 | -32.87% |
| ledger_audit | 4 | 166060 | 130877 | +26.88% |
| merge_ranges | 4 | 68794 | 69466 | -0.97% |
| repair_catalog | 4 | 63337 | 59948 | +5.65% |

**Falha preservada (DeepSeek JavaScript, rodada 4):** [20260905-121441Z-opencode-go-js_pagination](20260905-121441Z-opencode-go-js_pagination/slim.stdout.jsonl). Slim exit 12, repeated_failed_tool, oráculo 1; Pi passou. Uso Slim completo conhecido: 14031 input + 1461 output = 15492 tokens, registrados como tentativa malsucedida, fora da tabela de pares aprovados. O modelo repetiu write com expected sem a quebra final; o arquivo estava intacto e a proteção rejeitou corretamente. Reler e repetir o mesmo expected não resolveu. O resultado não foi substituído. Daily DeepSeek exit 1; Luna exit 0. Não houve recorrência da falha Responses nos 24 pares Luna desta bateria.

As reduções fixas aparecem no payload real: Luna system/schema bytes 1922/4785 → 1605/4424; tokens de entrada da primeira chamada caíram aproximadamente 118, com pequenas diferenças entre prompts. Isso não basta para demonstrar a meta ampla: os agregados por cenário continuam mistos e houve uma falha de conclusão de tarefa.

**Outro custo observado:** [DeepSeek ledger, rodada 4](20260905-120719Z-opencode-go-ledger_audit/summary.json) consumiu 76628 tokens no Slim. O arquivo contém ação, mas o stdout Python recebido pelo modelo contém U+FFFD; houve chamadas adicionais para examinar codepoints e validar os nomes. Reprodução local por pipe: Python herdado anunciou cp1252 e escreveu hex 61e7e36f0d0a; a leitura UTF-8 produz caracteres de substituição. Com PYTHONIOENCODING=utf-8 só no filho, anunciou utf-8 e escreveu 61c3a7c3a36f0d0a. O código do ProcessRunner não fixa a codificação do filho, e bounded_output usa from_utf8_lossy. Isso comprova a perda no texto mostrado; não atribui todos os tokens do outlier apenas a essa causa.


### Escrita protegida sem recópia e stdio UTF-8

O schema nativo de write passa a anunciar apenas path/content. A escrita de arquivo
existente exige leitura completa recente, e SHA-256 dos bytes observados continua
checado sob o bloqueio de mutação. O parser/API de precondição explícita conserva
seu contrato e continua rejeitando texto divergente; não foi removida a proteção.
Isso evita oferecer ao modelo a recópia integral do conteúdo antigo, que falhou
por omissão de newline no par DeepSeek JavaScript da bateria v1.6. A mensagem de
stale/precondition orienta reler e escrever só path/content ou aplicar patch exato.

ProcessRunner define PYTHONIOENCODING=utf-8 no processo filho quando não há valor
explicitamente configurado. Não altera o ambiente global nem a codificação dos
arquivos. Um override herdado é respeitado. O problema comprovado era Python em
pipe CP1252 tratado como UTF-8; isso não estabelece detecção universal de encodings
de qualquer executável legado. Prompt v1.7 recomenda parsers/serializers existentes
para dados estruturados; shell distingue edições de código, evitando induzir
transformações manuais de JSON. A recomendação não cita cenários ou modelos.

RED: três verificações falharam (schema ainda oferecia expected, erro sem caminho
de recuperação e processo filho sem default de stdio). GREEN: 12 native_tool_recovery
+ 25 tool_contracts. A regressão de EOF prova rejeição sem escrita, invalidação da
leitura após falha e recuperação após leitura completa com path/content. Os testes
de escrita parcial/obsoleta, guardas explícitas, CRLF, limites, cancelamento e saída
literal permanecem ativos. Um comando separado com PYTHONIOENCODING=cp1252 confirmou
que o override chega intacto ao filho. Logs %TEMP%/slim-native-stdio-write-{red,green}.log
e slim-native-stdio-override.log. O teste de contratos verifica os sete providers/dez
formatos de wire. Prefixo canônico offline: 1639 bytes de prompt + 3999 de schemas;
o formato de wire de cada adapter adiciona seu próprio envelope.

Nova bateria prospectiva fixada: quatro rodadas × seis cenários × dois modelos =
48 pares. Ordem de cenários inicia em config_migration/ledger_audit, depois json_cli,
merge_ranges, repair_catalog e js_pagination. Até dois cenários simultâneos por
modelo; braços sequenciais e alternados dentro do par. Rotas nativas, esforço high,
prompts/oráculos/fixtures inalterados. O binário será fixo durante toda a coleta;
falhas serão preservadas. Logs %TEMP%/slim-economy-guarded-{luna,deepseek}-{scenario}.log.


Gate completo: **1135 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings**;
Clippy --workspace --all-targets -- -D warnings exit 0; refresh-slim.ps1 -Test exit 0.
Target/PATH: 15102976 bytes, SHA-256 idêntico
`899EA9AFD74471218B50624FECF4E3F4EBA1C831C38407E845615A980F819339`.
Slim --version: slim 0.1.0, exit 0. Logs %TEMP%/slim-native-stdio-write-{gate,clippy}.log.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 09:33:06)
```


#### Resultado completo da bateria v1.7

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| luna | 1 | config_migration | 20377 | 28983 | 8/11 | [20260905-123416Z-config_migration](20260905-123416Z-config_migration/summary.json) |
| luna | 2 | config_migration | 11278 | 23566 | 6/9 | [20260905-123544Z-config_migration](20260905-123544Z-config_migration/summary.json) |
| luna | 3 | config_migration | 12338 | 10771 | 6/6 | [20260905-123700Z-config_migration](20260905-123700Z-config_migration/summary.json) |
| luna | 4 | config_migration | 31025 | 10800 | 12/5 | [20260905-123803Z-config_migration](20260905-123803Z-config_migration/summary.json) |
| luna | 1 | js_pagination | 9674 | 10689 | 5/5 | [20260905-124609Z-js_pagination](20260905-124609Z-js_pagination/summary.json) |
| luna | 2 | js_pagination | 9607 | 10664 | 5/5 | [20260905-124706Z-js_pagination](20260905-124706Z-js_pagination/summary.json) |
| luna | 3 | js_pagination | 9452 | 10680 | 5/5 | [20260905-124759Z-js_pagination](20260905-124759Z-js_pagination/summary.json) |
| luna | 4 | js_pagination | 9478 | 10746 | 5/5 | [20260905-124852Z-js_pagination](20260905-124852Z-js_pagination/summary.json) |
| luna | 1 | json_cli | 14420 | 21399 | 6/7 | [20260905-123844Z-json_cli](20260905-123844Z-json_cli/summary.json) |
| luna | 2 | json_cli | 12679 | 21923 | 5/7 | [20260905-124044Z-json_cli](20260905-124044Z-json_cli/summary.json) |
| luna | 3 | json_cli | 21875 | 12804 | 7/5 | [20260905-124229Z-json_cli](20260905-124229Z-json_cli/summary.json) |
| luna | 4 | json_cli | 16858 | 11896 | 6/5 | [20260905-124420Z-json_cli](20260905-124420Z-json_cli/summary.json) |
| luna | 1 | ledger_audit | 16217 | 14051 | 6/5 | [20260905-123416Z-ledger_audit](20260905-123416Z-ledger_audit/summary.json) |
| luna | 2 | ledger_audit | 16021 | 14415 | 6/5 | [20260905-123511Z-ledger_audit](20260905-123511Z-ledger_audit/summary.json) |
| luna | 3 | ledger_audit | 17387 | 21516 | 6/8 | [20260905-123607Z-ledger_audit](20260905-123607Z-ledger_audit/summary.json) |
| luna | 4 | ledger_audit | 16996 | 26914 | 6/8 | [20260905-123720Z-ledger_audit](20260905-123720Z-ledger_audit/summary.json) |
| luna | 1 | merge_ranges | 8987 | 8460 | 4/4 | [20260905-123926Z](20260905-123926Z/summary.json) |
| luna | 2 | merge_ranges | 8468 | 8799 | 4/4 | [20260905-124024Z](20260905-124024Z/summary.json) |
| luna | 3 | merge_ranges | 14826 | 8330 | 6/4 | [20260905-124119Z](20260905-124119Z/summary.json) |
| luna | 4 | merge_ranges | 9126 | 8653 | 4/4 | [20260905-124225Z](20260905-124225Z/summary.json) |
| luna | 1 | repair_catalog | 9249 | 11046 | 5/6 | [20260905-124321Z-repair_catalog](20260905-124321Z-repair_catalog/summary.json) |
| luna | 2 | repair_catalog | 11732 | 9552 | 6/5 | [20260905-124417Z-repair_catalog](20260905-124417Z-repair_catalog/summary.json) |
| luna | 3 | repair_catalog | 9003 | 11579 | 5/7 | [20260905-124512Z-repair_catalog](20260905-124512Z-repair_catalog/summary.json) |
| luna | 4 | repair_catalog | 8936 | 9691 | 5/5 | [20260905-124605Z-repair_catalog](20260905-124605Z-repair_catalog/summary.json) |

luna: 24/24 pares completos; 326009 / 337927 tokens (-3.53% Slim); mediana das razões por par -8.64%; 14/24 vitórias. Todos os oráculos passaram, fixtures intactos, uso completo, auditor e daily exit 0.

| Cenário | Slim | Pi | Variação Slim |
|---|---:|---:|---:|
| config_migration | 75018 | 74120 | +1.21% |
| js_pagination | 38211 | 42779 | -10.68% |
| json_cli | 65832 | 68022 | -3.22% |
| ledger_audit | 66621 | 76896 | -13.36% |
| merge_ranges | 41407 | 34242 | +20.92% |
| repair_catalog | 38920 | 41868 | -7.04% |

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Campanha |
|---|---:|---|---:|---:|---|---|
| deepseek | 1 | config_migration | 18458 | 18551 | 6/6 | [20260905-123426Z-opencode-go-config_migration](20260905-123426Z-opencode-go-config_migration/summary.json) |
| deepseek | 2 | config_migration | 23326 | 16507 | 7/5 | [20260905-123502Z-opencode-go-config_migration](20260905-123502Z-opencode-go-config_migration/summary.json) |
| deepseek | 3 | config_migration | 17632 | 15440 | 6/5 | [20260905-123540Z-opencode-go-config_migration](20260905-123540Z-opencode-go-config_migration/summary.json) |
| deepseek | 4 | config_migration | 18771 | 21037 | 6/6 | [20260905-123637Z-opencode-go-config_migration](20260905-123637Z-opencode-go-config_migration/summary.json) |
| deepseek | 1 | js_pagination | 15776 | 16091 | 5/5 | [20260905-124315Z-opencode-go-js_pagination](20260905-124315Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | js_pagination | 15735 | 15502 | 5/5 | [20260905-124355Z-opencode-go-js_pagination](20260905-124355Z-opencode-go-js_pagination/summary.json) |
| deepseek | 3 | js_pagination | 14888 | 17582 | 5/5 | [20260905-124425Z-opencode-go-js_pagination](20260905-124425Z-opencode-go-js_pagination/summary.json) |
| deepseek | 4 | js_pagination | 15887 | 13724 | 5/5 | [20260905-124524Z-opencode-go-js_pagination](20260905-124524Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | json_cli | 23103 | 32061 | 6/7 | [20260905-123727Z-opencode-go-json_cli](20260905-123727Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | json_cli | 19008 | 30660 | 6/7 | [20260905-123840Z-opencode-go-json_cli](20260905-123840Z-opencode-go-json_cli/summary.json) |
| deepseek | 3 | json_cli | 18314 | 37349 | 6/8 | [20260905-124003Z-opencode-go-json_cli](20260905-124003Z-opencode-go-json_cli/summary.json) |
| deepseek | 4 | json_cli | 21833 | 48140 | 6/7 | [20260905-124132Z-opencode-go-json_cli](20260905-124132Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | ledger_audit | 25709 | 20273 | 6/5 | [20260905-123426Z-opencode-go-ledger_audit](20260905-123426Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 2 | ledger_audit | 38453 | 25740 | 6/6 | [20260905-123510Z-opencode-go-ledger_audit](20260905-123510Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 3 | ledger_audit | 37896 | 29840 | 6/7 | [20260905-123744Z-opencode-go-ledger_audit](20260905-123744Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 4 | ledger_audit | 18904 | 23834 | 5/6 | [20260905-123900Z-opencode-go-ledger_audit](20260905-123900Z-opencode-go-ledger_audit/summary.json) |
| deepseek | 1 | merge_ranges | 16691 | 24281 | 5/6 | [20260905-123937Z-opencode-go](20260905-123937Z-opencode-go/summary.json) |
| deepseek | 2 | merge_ranges | 16178 | 16222 | 5/5 | [20260905-124031Z-opencode-go](20260905-124031Z-opencode-go/summary.json) |
| deepseek | 3 | merge_ranges | 22842 | 26810 | 6/6 | [20260905-124115Z-opencode-go](20260905-124115Z-opencode-go/summary.json) |
| deepseek | 4 | merge_ranges | 22536 | 18228 | 6/5 | [20260905-124222Z-opencode-go](20260905-124222Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 18730 | 15399 | 6/5 | [20260905-124256Z-opencode-go-repair_catalog](20260905-124256Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | repair_catalog | 16038 | 13606 | 6/5 | [20260905-124326Z-opencode-go-repair_catalog](20260905-124326Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 3 | repair_catalog | 14435 | 14455 | 5/5 | [20260905-124402Z-opencode-go-repair_catalog](20260905-124402Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 4 | repair_catalog | 13498 | 13646 | 5/5 | [20260905-124427Z-opencode-go-repair_catalog](20260905-124427Z-opencode-go-repair_catalog/summary.json) |

deepseek: 24/24 pares completos; 484641 / 524978 tokens (-7.68% Slim); mediana das razões por par -0.39%; 14/24 vitórias. Todos os oráculos passaram, fixtures intactos, uso completo, auditor e daily exit 0.

| Cenário | Slim | Pi | Variação Slim |
|---|---:|---:|---:|
| config_migration | 78187 | 71535 | +9.30% |
| js_pagination | 62286 | 62899 | -0.97% |
| json_cli | 82258 | 148210 | -44.50% |
| ledger_audit | 120962 | 99687 | +21.34% |
| merge_ranges | 78247 | 85541 | -8.53% |
| repair_catalog | 62701 | 57106 | +9.80% |

**Conclusão desta bateria:** 48/48 pares aprovados (96 execuções nativas), mas a economia ampla/considerável continua não demonstrada. Luna economizou no agregado de quatro dos seis cenários; DeepSeek, em três. No DeepSeek, JSON CLI concentra a vantagem, enquanto ledger, migração e reparo custaram mais. A mediana DeepSeek de -0,39% é praticamente empate. Não foi usado teste de significância, e quatro rodadas por cenário não estabelecem superioridade para qualquer tarefa/modelo/provider. Resultados de hashes anteriores não foram somados a estes.

A correção de stdio foi confirmada em execuções reais: [Luna ledger](20260905-123416Z-ledger_audit/summary.json), [Luna ledger seguinte](20260905-123511Z-ledger_audit/summary.json) e [DeepSeek ledger](20260905-123426Z-opencode-go-ledger_audit/summary.json) mostram ação intacto no stdout Python. A inspeção dos 48 pares não encontrou U+FFFD nos outputs Slim nem falhas do tool write. Há falhas intermediárias corrigidas de shell/patch/read/list, portanto aprovação dos oráculos não significa ausência de qualquer erro intermediário. Não houve falha Responses de identidade nesta bateria.

Validação da entrega: cargo test --workspace (via refresh -Test) 1135 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings; Clippy -D warnings exit 0; deploy/versão e hash alvo/PATH conferidos. Diff-check passou. ConPTY físico e ZIP não se aplicam a esta alteração de core. A qualidade comprovada é a dos testes/oráculos executados, sem alegação de equivalência universal. **A meta de economia ampla permanece aberta.**

## Experimento de descoberta inicial — protótipo, sem mudança no runtime

Após a bateria v1.7, os traces mostraram prefixos de duas/três requisições com apenas read/list/search em muitos casos. Para testar a hipótese antes de acrescentar infraestrutura ao produto, foi iniciado um A/B entre o mesmo Slim instalado (899EA9AFD74471218B50624FECF4E3F4EBA1C831C38407E845615A980F819339) com o prompt original e com uma lista inicial curta de caminhos. Este experimento não é uma comparação com o Pi nem uma funcionalidade já entregue.

Plano fixado antes do término: duas rodadas, seis cenários existentes e dois modelos/rotas nativas (Luna/openai-codex e DeepSeek/opencode-go), total 24 pares/48 execuções. Ordem baseline/snapshot alternada por cenário e rodada, uma execução por modelo de cada vez. Fixtures, pedido de trabalho, esforço high, speed normal e oráculo externo são os mesmos. A única intervenção é o contexto experimental acrescentado ao prompt Slim; o manifesto e effective-slim-prompt.txt registram exatamente o texto enviado. Todos os resultados, inclusive falhas, serão retidos.

O protótipo lista nomes relativos em profundidade até 2, com teto de 64 caminhos, 128 entradas examinadas, 1536 bytes no array JSON e orçamento cooperativo de 100 ms. Exclui nomes ocultos, links simbólicos e diretórios node_modules/target/dist/.git/.slim/.pi; não lê os conteúdos dos arquivos. A lista é explicitamente parcial e trata nomes como dados, seguida de orientação para agrupar leituras relevantes. Não implementa gitignore nem garantias de cancelamento/latência de produção: é um experimento sobre fixtures controlados. Cada campanha guarda uma cópia exata do script discovery_probe.py para reprodução. O tempo cooperativo não interrompe uma operação de sistema bloqueada.

### Resultado completo do A/B de descoberta

As duas baterias encerraram com exit 0. O auditor conferiu 48 execuções / 24 pares, uso nativo completo, hashes iguais, mesmas rotas/modelos/esforço, pedido de trabalho idêntico, prompts efetivos registrados e fixtures/oráculos preservados. Todos os 48 oráculos externos passaram. [Agregado e pares completos](discovery-probe-summary.json).

| Modelo | Slim atual | Slim + contexto experimental | Variação | Mediana por par | Pares com economia |
|---|---:|---:|---:|---:|---:|
| deepseek-v4-flash | 215437 | 202648 | -5.94% | -2.63% | 8/12 |
| gpt-5.6-luna | 163812 | 147351 | -10.05% | -8.58% | 9/12 |

| Modelo | Chamadas totais atual → protótipo | Chamadas iniciais de descoberta | Listagens da raiz |
|---|---:|---:|---:|
| deepseek-v4-flash | 64 → 54 | 25 → 15 | 10 → 2 |
| gpt-5.6-luna | 71 → 60 | 25 → 15 | 13 → 5 |

A intervenção adicionou 204–291 bytes de texto conforme o workspace. A primeira entrada aumentou 43–63 tokens no Luna e 44–66 no DeepSeek, sempre contabilizados. O ganho observado veio da redução de interações e de seu histórico repetido; o contexto inicial não é gratuito. Contagem de descoberta: prefixo consecutivo de requisições que contêm somente read/list/search; não equivale a todo trabalho de investigação.

| Modelo | Cenário | Atual | Protótipo | Variação |
|---|---|---:|---:|---:|
| deepseek-v4-flash | config_migration | 35876 | 33879 | -5.57% |
| deepseek-v4-flash | js_pagination | 29558 | 29222 | -1.14% |
| deepseek-v4-flash | json_cli | 39975 | 42539 | +6.41% |
| deepseek-v4-flash | ledger_audit | 42732 | 44437 | +3.99% |
| deepseek-v4-flash | merge_ranges | 38376 | 28589 | -25.50% |
| deepseek-v4-flash | repair_catalog | 28920 | 23982 | -17.07% |
| gpt-5.6-luna | config_migration | 47733 | 40740 | -14.65% |
| gpt-5.6-luna | js_pagination | 18483 | 17974 | -2.75% |
| gpt-5.6-luna | json_cli | 27706 | 26744 | -3.47% |
| gpt-5.6-luna | ledger_audit | 33014 | 30757 | -6.84% |
| gpt-5.6-luna | merge_ranges | 18922 | 16340 | -13.65% |
| gpt-5.6-luna | repair_catalog | 17954 | 14796 | -17.59% |

| Modelo | Rodada | Cenário | Atual | Protótipo | Variação | Evidências |
|---|---:|---|---:|---:|---:|---|
| gpt-5.6-luna | 1 | merge_ranges | 8746 | 7946 | -9.15% | [atual](20260905-131258Z/probe-summary.json), [protótipo](20260905-131325Z/probe-summary.json) |
| gpt-5.6-luna | 1 | repair_catalog | 9052 | 7408 | -18.16% | [atual](20260905-131404Z-repair_catalog/probe-summary.json), [protótipo](20260905-131346Z-repair_catalog/probe-summary.json) |
| gpt-5.6-luna | 1 | json_cli | 14134 | 13091 | -7.38% | [atual](20260905-131423Z-json_cli/probe-summary.json), [protótipo](20260905-131510Z-json_cli/probe-summary.json) |
| gpt-5.6-luna | 1 | js_pagination | 9307 | 9817 | +5.48% | [atual](20260905-131614Z-js_pagination/probe-summary.json), [protótipo](20260905-131554Z-js_pagination/probe-summary.json) |
| gpt-5.6-luna | 1 | ledger_audit | 16401 | 15476 | -5.64% | [atual](20260905-131637Z-ledger_audit/probe-summary.json), [protótipo](20260905-131702Z-ledger_audit/probe-summary.json) |
| gpt-5.6-luna | 1 | config_migration | 12712 | 21535 | +69.41% | [atual](20260905-131757Z-config_migration/probe-summary.json), [protótipo](20260905-131725Z-config_migration/probe-summary.json) |
| gpt-5.6-luna | 2 | merge_ranges | 10176 | 8394 | -17.51% | [atual](20260905-131844Z/probe-summary.json), [protótipo](20260905-131822Z/probe-summary.json) |
| gpt-5.6-luna | 2 | repair_catalog | 8902 | 7388 | -17.01% | [atual](20260905-131918Z-repair_catalog/probe-summary.json), [protótipo](20260905-131936Z-repair_catalog/probe-summary.json) |
| gpt-5.6-luna | 2 | json_cli | 13572 | 13653 | +0.60% | [atual](20260905-132044Z-json_cli/probe-summary.json), [protótipo](20260905-131956Z-json_cli/probe-summary.json) |
| gpt-5.6-luna | 2 | js_pagination | 9176 | 8157 | -11.11% | [atual](20260905-132142Z-js_pagination/probe-summary.json), [protótipo](20260905-132210Z-js_pagination/probe-summary.json) |
| gpt-5.6-luna | 2 | ledger_audit | 16613 | 15281 | -8.02% | [atual](20260905-132258Z-ledger_audit/probe-summary.json), [protótipo](20260905-132228Z-ledger_audit/probe-summary.json) |
| gpt-5.6-luna | 2 | config_migration | 35021 | 19205 | -45.16% | [atual](20260905-132342Z-config_migration/probe-summary.json), [protótipo](20260905-132427Z-config_migration/probe-summary.json) |
| deepseek-v4-flash | 1 | merge_ranges | 17662 | 15295 | -13.40% | [atual](20260905-131258Z-opencode-go/probe-summary.json), [protótipo](20260905-131325Z-opencode-go/probe-summary.json) |
| deepseek-v4-flash | 1 | repair_catalog | 14862 | 10967 | -26.21% | [atual](20260905-131451Z-opencode-go-repair_catalog/probe-summary.json), [protótipo](20260905-131434Z-opencode-go-repair_catalog/probe-summary.json) |
| deepseek-v4-flash | 1 | json_cli | 17099 | 19652 | +14.93% | [atual](20260905-131512Z-opencode-go-json_cli/probe-summary.json), [protótipo](20260905-131536Z-opencode-go-json_cli/probe-summary.json) |
| deepseek-v4-flash | 1 | js_pagination | 14256 | 14221 | -0.25% | [atual](20260905-131623Z-opencode-go-js_pagination/probe-summary.json), [protótipo](20260905-131605Z-opencode-go-js_pagination/probe-summary.json) |
| deepseek-v4-flash | 1 | ledger_audit | 18899 | 21388 | +13.17% | [atual](20260905-131643Z-opencode-go-ledger_audit/probe-summary.json), [protótipo](20260905-131703Z-opencode-go-ledger_audit/probe-summary.json) |
| deepseek-v4-flash | 1 | config_migration | 18514 | 13890 | -24.98% | [atual](20260905-131804Z-opencode-go-config_migration/probe-summary.json), [protótipo](20260905-131751Z-opencode-go-config_migration/probe-summary.json) |
| deepseek-v4-flash | 2 | merge_ranges | 20714 | 13294 | -35.82% | [atual](20260905-131849Z-opencode-go/probe-summary.json), [protótipo](20260905-131829Z-opencode-go/probe-summary.json) |
| deepseek-v4-flash | 2 | repair_catalog | 14058 | 13015 | -7.42% | [atual](20260905-131913Z-opencode-go-repair_catalog/probe-summary.json), [protótipo](20260905-131926Z-opencode-go-repair_catalog/probe-summary.json) |
| deepseek-v4-flash | 2 | json_cli | 22876 | 22887 | +0.05% | [atual](20260905-132008Z-opencode-go-json_cli/probe-summary.json), [protótipo](20260905-131942Z-opencode-go-json_cli/probe-summary.json) |
| deepseek-v4-flash | 2 | js_pagination | 15302 | 15001 | -1.97% | [atual](20260905-132101Z-opencode-go-js_pagination/probe-summary.json), [protótipo](20260905-132121Z-opencode-go-js_pagination/probe-summary.json) |
| deepseek-v4-flash | 2 | ledger_audit | 23833 | 23049 | -3.29% | [atual](20260905-132218Z-opencode-go-ledger_audit/probe-summary.json), [protótipo](20260905-132153Z-opencode-go-ledger_audit/probe-summary.json) |
| deepseek-v4-flash | 2 | config_migration | 17362 | 19989 | +15.13% | [atual](20260905-132305Z-opencode-go-config_migration/probe-summary.json), [protótipo](20260905-132319Z-opencode-go-config_migration/probe-summary.json) |

Limitações e decisão: há um sinal favorável em ambos os modelos, com redução de dez chamadas iniciais em cada um. No Luna, os seis agregados de cenário economizaram; no DeepSeek, quatro, com JSON CLI +6,41% e ledger +3,99%. Duas rodadas por cenário e fixtures pequenos não demonstram ganho universal nem comportamento em repositórios grandes. A orientação de agrupar leituras acompanha a lista, portanto o A/B mede a intervenção conjunta.

A falha intermediária na migração Luna da primeira rodada foi retida: o protótipo produziu um patch com vírgula ausente, omitiu uma transformação e chamou shell com command="python check.py", args=[]; o modelo corrigiu e o oráculo passou. A baseline da mesma rodada também errou a forma command/args e recuperou. [Trace do protótipo](20260905-131725Z-config_migration/slim.session.jsonl), [baseline](20260905-131757Z-config_migration/slim.session.jsonl). Esses custos não foram removidos para favorecer o resultado.

**Não somar esses percentuais aos ganhos anteriores contra o Pi. Este A/B compara Slim contra o próprio Slim, com contexto experimental adicional. O runtime e o binário instalado continuam inalterados e a meta de superar amplamente o Pi permanece aberta.** O resultado justifica avaliar uma integração limitada e validar sua economia contra o Pi com contextos de maior escala; ainda faltam controle de orçamento de contexto, respeito a ignore e testes de retomada/cancelamento para uma implementação de produção.

Verificação desta etapa: processos Luna e DeepSeek exit 0; auditor A/B exit 0 (AUDIT_OK 48 runs, 24 paired comparisons); SHA256 instalado reconferido igual ao registrado. Não houve mudança de código do produto, portanto cargo test/build/deploy e atualização de contagens de testes do produto não se aplicam. A evidência nova são os benchmarks e seus oráculos; não foram reutilizados testes antigos como se fossem executados agora.

Checklist RULES §4 desta etapa experimental:
- [x] Comandos executados e resultados registrados: duas baterias exit 0; auditor AUDIT_OK 48 runs, 24 paired comparisons.
- [x] Números calculados dos 48 registros nativos desta etapa, com uso completo.
- [x] Caminhos de evidência lidos; links locais do relatório validados sem alvo ausente.
- [x] cargo test --workspace: não aplicável; nenhum código do produto alterado.
- [x] refresh-slim.ps1: não aplicável; executável instalado inalterado, SHA256 conferido.
- [x] Relatório de benchmark atualizado; contagens de testes do produto inalteradas, sem novo build a publicar.
- [x] Limitações e escopo experimental declarados; comparação atual é Slim/Slim, não Slim/Pi.

## Contexto inicial integrado ao runtime — comparação nativa

O runtime agora inclui uma lista parcial de caminhos na primeira mensagem de uma conversa nova. O walker existente respeita .gitignore/.ignore, excludes/global ignore, arquivos ocultos e os diretórios descartados pela busca; não segue links/junctions. Limites: profundidade 2, 64 caminhos, array JSON até 1536 bytes, 128 entradas retornadas pelo walker e deadline cooperativo de 100 ms (não interrompe I/O bloqueado ou carregamento de ignore). Apenas nomes, nunca corpos de arquivos; a lista não autoriza sobrescrita.

O contexto adicional passa pela redação de valores sensíveis e pelo orçamento completo de request. É descartado caso cruze o limiar suave de compactação. Não é acrescentado em retomadas ou continuations, não duplica uma lista já presente e preserva blocos anexados. APIs de request direto mantêm sua semântica; o recurso pertence ao loop do agente. Falta de snapshot não prova ausência de arquivos.

Evidência de integração: cinco novos testes focados, incluindo junction real no Windows, ignore/depth/bytes/quantidade, cancelamento, retomada/anexos/redação, orçamento e proteção de escrita. O teste de contabilização agora compara contra o corpo HTTP capturado, incluindo o contexto real. Os asserts de E2E continuam exigindo pedido original exato e verificando segredo/uso/sessão; apenas reconhecem o contexto adicional do runtime. Gate refresh -Test: 1140 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings.

Build instalado: 05/09/2026 10:51:43, 15132160 bytes. SHA256 alvo/PATH: 016046F6570E6684E9AEF7F3613E901ECC57BE2107A42C1084A682C8D60D1DB7.

Plano fixado antes da medição nativa: duas rodadas por cenário/modelo, os seis cenários existentes mais dois casos ainda não medidos (repositório maior e consulta SQLite), total 32 pares / 64 execuções. Luna/openai-codex e DeepSeek/opencode-go, esforço high e speed normal; alternância da ordem dos braços, sem alterar prompts/fixtures/oráculos entre Slim e Pi. Todos os resultados serão retidos. A comparação usa o Slim instalado, sem o script experimental de injeção de prompt. Bateria concluída; resultados e limitações abaixo.

### Resultado completo do contexto inicial integrado

As 32 comparações planejadas terminaram: 30/32 pares completos e aprovados, 62/64 braços aprovados. Os dois casos restantes são falhas de transporte Slim/DeepSeek em ledger_audit; Pi passou nos dois. O uso desses requests Slim é desconhecido, não zero. Portanto, o agregado DeepSeek abaixo corresponde somente aos 14 pares completos e não representa todo o custo das 16 tentativas. [Agregado, status e pares](initial-paths-native-summary.json).

| Modelo | Pares completos | Tokens Slim | Tokens Pi | Variação Slim | Mediana por par | Pares com economia | Chamadas Slim/Pi |
|---|---:|---:|---:|---:|---:|---:|---:|
| luna | 16/16 | 225894 | 202226 | +11.70% | -7.20% | 9/16 | 79/83 |
| deepseek | 14/16 | 272877 | 298303 | -8.52% | -10.15% | 10/14 | 67/79 |

| Modelo | Cenário | Pares completos | Slim | Pi | Variação Slim |
|---|---|---:|---:|---:|---:|
| luna | config_migration | 2/2 | 16568 | 24567 | -32.56% |
| luna | js_pagination | 2/2 | 15842 | 22560 | -29.78% |
| luna | json_cli | 2/2 | 59545 | 29363 | +102.79% |
| luna | ledger_audit | 2/2 | 46697 | 31551 | +48.00% |
| luna | merge_ranges | 2/2 | 15893 | 17120 | -7.17% |
| luna | repair_catalog | 2/2 | 15050 | 20843 | -27.79% |
| luna | repo_wide_repair | 2/2 | 32928 | 31033 | +6.11% |
| luna | sqlite_balances | 2/2 | 23371 | 25189 | -7.22% |
| deepseek | config_migration | 2/2 | 32425 | 32004 | +1.32% |
| deepseek | js_pagination | 2/2 | 27201 | 31709 | -14.22% |
| deepseek | json_cli | 2/2 | 49503 | 43190 | +14.62% |
| deepseek | ledger_audit | 0/2 | uso incompleto | par excluído | não calculável |
| deepseek | merge_ranges | 2/2 | 32997 | 41395 | -20.29% |
| deepseek | repair_catalog | 2/2 | 25779 | 29598 | -12.90% |
| deepseek | repo_wide_repair | 2/2 | 67226 | 56403 | +19.19% |
| deepseek | sqlite_balances | 2/2 | 37746 | 64004 | -41.03% |

Os casos inéditos foram definidos antes de sua medição em [holdouts.py](holdouts.py). repo_wide_repair contém 85 arquivos de entrada (116491 bytes), incluindo 80 arquivos históricos sem relação com a tarefa e implementação em profundidade 3; verifica cálculo inteiro, validação, caller, entrada imutável e 84 arquivos preservados. sqlite_balances verifica multiplicação indevida de joins, pagamentos múltiplos, excesso de pagamento isolado por pedido, cancelamento e clientes/pedidos vazios. Os dois oráculos rejeitaram o fixture defeituoso (exit 1) e aprovaram a referência (exit 0) antes das chamadas nativas. As referências ficaram fora dos workspaces dos agentes.

| Modelo | Rodada | Cenário | Slim | Pi | Chamadas Slim/Pi | Evidência |
|---|---:|---|---:|---:|---:|---|
| luna | 1 | merge_ranges | 7665 | 8410 | 4/4 | [20260905-135459Z](20260905-135459Z/summary.json) |
| luna | 1 | repair_catalog | 7793 | 10993 | 4/6 | [20260905-135551Z-repair_catalog](20260905-135551Z-repair_catalog/summary.json) |
| luna | 1 | json_cli | 13301 | 12360 | 5/5 | [20260905-135644Z-json_cli](20260905-135644Z-json_cli/summary.json) |
| luna | 1 | js_pagination | 8035 | 12132 | 4/6 | [20260905-135822Z-js_pagination](20260905-135822Z-js_pagination/summary.json) |
| luna | 1 | ledger_audit | 30437 | 17691 | 8/6 | [20260905-135916Z-ledger_audit](20260905-135916Z-ledger_audit/summary.json) |
| luna | 1 | config_migration | 8322 | 9434 | 4/5 | [20260905-140030Z-config_migration](20260905-140030Z-config_migration/summary.json) |
| luna | 2 | merge_ranges | 8228 | 8710 | 4/4 | [20260905-140126Z](20260905-140126Z/summary.json) |
| luna | 2 | repair_catalog | 7257 | 9850 | 4/5 | [20260905-140223Z-repair_catalog](20260905-140223Z-repair_catalog/summary.json) |
| luna | 2 | json_cli | 46244 | 17003 | 10/6 | [20260905-140307Z-json_cli](20260905-140307Z-json_cli/summary.json) |
| luna | 2 | js_pagination | 7807 | 10428 | 4/5 | [20260905-140540Z-js_pagination](20260905-140540Z-js_pagination/summary.json) |
| luna | 2 | ledger_audit | 16260 | 13860 | 5/5 | [20260905-140628Z-ledger_audit](20260905-140628Z-ledger_audit/summary.json) |
| luna | 2 | config_migration | 8246 | 15133 | 4/6 | [20260905-140736Z-config_migration](20260905-140736Z-config_migration/summary.json) |
| deepseek | 1 | merge_ranges | 17926 | 19769 | 5/6 | [20260905-135459Z-opencode-go](20260905-135459Z-opencode-go/summary.json) |
| deepseek | 1 | repair_catalog | 12573 | 14962 | 4/5 | [20260905-135537Z-opencode-go-repair_catalog](20260905-135537Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 1 | json_cli | 30436 | 19127 | 7/6 | [20260905-135616Z-opencode-go-json_cli](20260905-135616Z-opencode-go-json_cli/summary.json) |
| deepseek | 1 | js_pagination | 14816 | 16728 | 4/5 | [20260905-135819Z-opencode-go-js_pagination](20260905-135819Z-opencode-go-js_pagination/summary.json) |
| deepseek | 1 | ledger_audit | falha/uso incompleto | aprovado | não comparável | [20260905-135906Z-opencode-go-ledger_audit](20260905-135906Z-opencode-go-ledger_audit/failure-summary.json) |
| deepseek | 1 | config_migration | 16738 | 14471 | 5/5 | [20260905-135950Z-opencode-go-config_migration](20260905-135950Z-opencode-go-config_migration/summary.json) |
| deepseek | 2 | merge_ranges | 15071 | 21626 | 4/5 | [20260905-140034Z-opencode-go](20260905-140034Z-opencode-go/summary.json) |
| deepseek | 2 | repair_catalog | 13206 | 14636 | 4/5 | [20260905-140143Z-opencode-go-repair_catalog](20260905-140143Z-opencode-go-repair_catalog/summary.json) |
| deepseek | 2 | json_cli | 19067 | 24063 | 5/6 | [20260905-140215Z-opencode-go-json_cli](20260905-140215Z-opencode-go-json_cli/summary.json) |
| deepseek | 2 | js_pagination | 12385 | 14981 | 4/5 | [20260905-140325Z-opencode-go-js_pagination](20260905-140325Z-opencode-go-js_pagination/summary.json) |
| deepseek | 2 | ledger_audit | falha/uso incompleto | aprovado | não comparável | [20260905-140443Z-opencode-go-ledger_audit](20260905-140443Z-opencode-go-ledger_audit/failure-summary.json) |
| deepseek | 2 | config_migration | 15687 | 17533 | 5/6 | [20260905-140516Z-opencode-go-config_migration](20260905-140516Z-opencode-go-config_migration/summary.json) |
| luna | 1 | repo_wide_repair | 15359 | 15026 | 5/5 | [20260905-140830Z-repo_wide_repair](20260905-140830Z-repo_wide_repair/summary.json) |
| luna | 1 | sqlite_balances | 9666 | 12662 | 4/5 | [20260905-140944Z-sqlite_balances](20260905-140944Z-sqlite_balances/summary.json) |
| luna | 2 | repo_wide_repair | 17569 | 16007 | 5/5 | [20260905-141039Z-repo_wide_repair](20260905-141039Z-repo_wide_repair/summary.json) |
| luna | 2 | sqlite_balances | 13705 | 12527 | 5/5 | [20260905-141156Z-sqlite_balances](20260905-141156Z-sqlite_balances/summary.json) |
| deepseek | 1 | repo_wide_repair | 32810 | 28448 | 6/6 | [20260905-140617Z-opencode-go-repo_wide_repair](20260905-140617Z-opencode-go-repo_wide_repair/summary.json) |
| deepseek | 1 | sqlite_balances | 17175 | 42959 | 4/8 | [20260905-140725Z-opencode-go-sqlite_balances](20260905-140725Z-opencode-go-sqlite_balances/summary.json) |
| deepseek | 2 | repo_wide_repair | 34416 | 27955 | 6/6 | [20260905-140920Z-opencode-go-repo_wide_repair](20260905-140920Z-opencode-go-repo_wide_repair/summary.json) |
| deepseek | 2 | sqlite_balances | 20571 | 21045 | 4/5 | [20260905-141029Z-opencode-go-sqlite_balances](20260905-141029Z-opencode-go-sqlite_balances/summary.json) |

Os dois erros de transporte ocorreram aos 15002 e 15014 ms, sem primeiro byte no request que falhou. No primeiro caso houve uma rodada anterior de leitura bem-sucedida; no segundo o request inicial falhou. O código daquela execução aplicava timeouts.connect à operação send inteira, incluindo upload e espera de cabeçalhos, e a política de produção limitava esse campo a 15 segundos. Isso demonstra que o prazo curto pode cortar a espera do servidor; os logs atuais não distinguem espera de conexão, upload ou cabeçalhos nessas duas falhas. Não houve retry automático nem substituição de amostras.

O Luna teve custos elevados em ledger (uma chamada shell sem command, seguida de correção de JSON gerado) e JSON CLI da segunda rodada (mudança após o primeiro check, novas verificações e limpeza de __pycache__). [Ledger](20260905-135916Z-ledger_audit/summary.json), [JSON CLI](20260905-140307Z-json_cli/summary.json). Esses percursos foram incluídos integralmente. Não é correto transformar a redução de chamadas ou a mediana favorável em economia total: Luna terminou +11,70%.

**Conclusão:** a integração funcional está validada, mas a meta de superar amplamente o Pi não foi atingida. O repositório maior custou +6,11% no Luna e +19,19% no DeepSeek; a consulta SQL economizou nos dois. No conjunto, os ganhos continuam desiguais e há duas falhas de transporte com uso incompleto. A etapa anterior Slim/Slim continua sendo um experimento separado, não uma parcela a somar a estes percentuais. Não houve teste de significância nem alegação de validade para qualquer provider/modelo.

Validação do código instalado: refresh-slim.ps1 -Test exit 0, 1140 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings; Clippy --workspace --all-targets -- -D warnings exit 0; versão 0.1.0 e hashes alvo/PATH iguais. OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 10:51:43). ConPTY físico e ZIP não se aplicam ao escopo.

Checklist RULES §4 desta integração:
- [x] Comandos executados: refresh -Test exit 0, Clippy -D warnings exit 0, auditor nativo concluído; falhas de benchmark retidas.
- [x] Números contados desta execução: 1140 passed; 32 pares planejados, 30 aprovados e 2 falhas com uso incompleto.
- [x] Código e caminhos citados lidos; links locais do relatório verificados sem alvo ausente.
- [x] cargo test --workspace verde: 0 failed / 0 compiler warnings / 1 ignored / 73 suítes.
- [x] refresh-slim.ps1 -Test imprimiu o OK transcrito; alvo/PATH possuem SHA256 idêntico.
- [x] Cinco documentos de status e tracker §7 atualizados; metadados atuais anteriores ausentes nesses cinco documentos. Diff-check exit 0.
- [x] Limitações declaradas: objetivo amplo não atingido; duas falhas de transporte, ganho desigual, ConPTY físico e ZIP não aplicáveis.


## Contexto equilibrado e prazos HTTP

Mudanças de 2026-09-05 baseadas no código e em reproduções locais. O prazo connect limitava toda a operação HTTP send, incluindo cabeçalhos: a reprodução com TCP aceito falhou após 48,8836 ms, antes do orçamento semântico de 150 ms; uma resposta válida com cabeçalhos após 200 ms também falhou. Agora reqwest limita TCP/TLS e a espera de upload/cabeçalhos usa o menor prazo idle/first_semantic. A conexão compartilhada conserva a política connect específica em um cache limitado de 16 perfis; cancelamento/wall e proibição de retry ambíguo do POST permanecem. O teste de TLS usa servidor local que aceita TCP e não completa o handshake, cobrindo clientes normais e compartilhados. Isso prova o erro de deadline, mas não identifica a fase exata das duas falhas remotas anteriores.

A listagem inicial antiga percorreu 80 arquivos de uma pasta e omitiu até README.md. O teste reproduziu essa omissão. A implementação faz percursos rasos em largura: raiz primeiro, no máximo 8 caminhos por diretório não raiz e profundidade global 3. Mantém 64 caminhos, 1536 bytes de array JSON, 128 entradas retornadas pelos walkers e prazo cooperativo de 100 ms. Cada diretório enfileirado é revalidado contra links/reparse points antes de abri-lo; ignores ancestrais continuam ativos. É uma lista parcial, sem leitura de corpos, sem garantia de snapshot atômico ou interrupção de I/O bloqueado. O limite não garante cobertura de toda árvore larga.

Código: [workspace.rs](../../crates/slim-core/src/runtime/workspace.rs), [provider.rs](../../crates/slim-core/src/provider.rs), [testes HTTP](../../crates/slim-core/tests/provider_http.rs). As duas reproduções ficaram verdes; transporte/cancelamento 68 testes, contexto 6 testes. Gate refresh -Test: 1142 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings; Clippy workspace all-targets -D warnings exit 0. Build 2026-09-05 11:51:29, 15136768 bytes, SHA256 alvo/PATH 6E671C9628029DB0154B67F0E7420534E7D4FCAE290B62D31F78176F90BA5E4E.

OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/05/2026 11:51:29).

Plano fixado antes de medir: duas rodadas dos oito cenários já definidos em daily.py e holdouts.py, em cada modelo, 32 pares / 64 braços. Mesmos prompts, fixtures, oráculos externos, high/normal e rotas nativas; ordem alternada por rodada/cenário. Sem reiniciar ou substituir tentativas que falhem. A bateria terminou; todos os resultados e as limitações estão abaixo. ConPTY físico e ZIP são não aplicáveis ao escopo core/CLI.


### Resultado completo da bateria equilibrada

As 32 comparações planejadas terminaram: 32/32 pares e 64/64 braços aprovados nos oráculos externos, com fixtures preservados e uso completo. Não houve substituição de amostras, falha de transporte ou retry nas 148 requisições Slim. As execuções nativas daily.py/holdouts.py encerraram com exit 0 nas duas rotas. [Todos os pares, agregados e hash](balanced-context-native-summary.json).

Tokens = entrada total, incluindo cache, mais saída informada pelo provider; reasoning não é somado novamente à saída. Isso mede consumo de tokens, não preço faturado. High/normal e as rotas nativas permanecem iguais às baterias anteriores.

| Modelo | Pares aprovados | Slim tokens | Pi tokens | Delta Slim/Pi | Vitórias por par | Chamadas Slim/Pi | Cenários com menor total |
|---|---:|---:|---:|---:|---:|---:|---:|
| luna | 16/16 | 190197 | 213138 | -10.76% | 13/16 | 74/84 | 6/8 |
| deepseek | 16/16 | 303246 | 385374 | -21.31% | 14/16 | 74/93 | 7/8 |

Cada linha abaixo agrega as duas rodadas do cenário; valores positivos significam maior consumo do Slim.

| Cenário | Luna Slim/Pi | Delta | DeepSeek Slim/Pi | Delta |
|---|---:|---:|---:|---:|
| config_migration | 30373/28276 | +7.42% | 30779/56087 | -45.12% |
| js_pagination | 16042/21348 | -24.85% | 24833/32791 | -24.27% |
| json_cli | 39555/34601 | +14.32% | 40570/64258 | -36.86% |
| ledger_audit | 30113/34359 | -12.36% | 57123/57629 | -0.88% |
| merge_ranges | 16124/16755 | -3.77% | 33040/45499 | -27.38% |
| repair_catalog | 14817/19565 | -24.27% | 24694/29084 | -15.09% |
| repo_wide_repair | 23596/33090 | -28.69% | 43189/58994 | -26.79% |
| sqlite_balances | 19577/25144 | -22.14% | 49018/41032 | +19.46% |

A correção de prazo também foi exercitada pelo provider real: os cabeçalhos do DeepSeek chegaram após **28114 ms** no primeiro request de [20260905-145618Z-opencode-go-config_migration](20260905-145618Z-opencode-go-config_migration/slim.session.jsonl) e após **54374 ms** no quarto request de [20260905-150659Z-opencode-go-sqlite_balances](20260905-150659Z-opencode-go-sqlite_balances/slim.session.jsonl). As respostas concluíram com sucesso e sem retry. O limite anterior de 15 s na operação send inteira interromperia essas esperas. Isso não identifica retroativamente a fase das duas falhas da bateria anterior.

No repositório maior, a entrada da primeira requisição caiu de 1395 para 1159 tokens no Luna e de 2305 para 1983 no DeepSeek, comparando as duas rodadas de cada versão. O histórico inicial serializado caiu de 2078 para 853 bytes no Luna e de 2047 para 822 no DeepSeek. Essa redução específica é consistente com a lista limitada por pasta. [Luna anterior](20260905-140830Z-repo_wide_repair/summary.json), [Luna atual](20260905-150537Z-repo_wide_repair/summary.json), [DeepSeek anterior](20260905-140617Z-opencode-go-repo_wide_repair/summary.json), [DeepSeek atual](20260905-150239Z-opencode-go-repo_wide_repair/summary.json). Os totais Slim/Pi do cenário ficaram -28,69% no Luna e -26,79% no DeepSeek nesta bateria.

As perdas continuam visíveis. Em [configuração Luna](20260905-145754Z-config_migration/summary.json), command="python check.py" com args=[] selecionou execução literal e falhou; após corrigir a chamada, a validação encontrou JSON inválido, exigindo outra edição. O contrato atual de shell distingue comando PowerShell de executável com argumentos. Em [SQLite DeepSeek](20260905-150427Z-opencode-go-sqlite_balances/summary.json), a primeira query usou coluna o.id inexistente naquele escopo, e o agente precisou corrigir a query e testar novamente. Os custos dessas falhas recuperadas estão incluídos: Slim teve 5 erros de ferramentas no conjunto, Pi 7; todos terminaram aprovados. JSON CLI Luna também ficou +14,32% no agregado do cenário.

**Conclusão verificada:** o Slim atual superou o Pi no consumo total desta bateria, nos dois modelos, com aprovação integral dos oráculos. A vantagem aparece em 6/8 cenários Luna e 7/8 DeepSeek, e em 27/32 pares. Isso é evidência mais ampla que um caso isolado, mas não prova vantagem em todo agente/provider, nem ausência de perda de qualidade fora dos contratos testados. São duas rodadas por cenário/modelo; não houve teste de significância. As duas mudanças foram medidas juntas contra o Pi, sem A/B contemporâneo contra o binário anterior: a variação dos percursos dos modelos impede atribuir todo o ganho agregado exclusivamente a elas. A meta universal permanece não demonstrada.

Checklist RULES §4 desta etapa:

- [x] Comandos executados: refresh-slim.ps1 -Test exit 0; Clippy --workspace --all-targets -- -D warnings exit 0; daily.py e holdouts.py concluídos; auditor dos 32 pares exit 0.
- [x] Números contados nesta execução: 1142 passed / 0 failed / 1 ignored / 73 suítes / 0 compiler warnings; 32 pares, 64 braços, 148 requests Slim.
- [x] Código e caminhos de evidência lidos; links locais do relatório verificados.
- [x] cargo test --workspace verde pelo refresh -Test, sem filtro de testes.
- [x] Refresh imprimiu o OK acima; executável de release e PATH têm o SHA256 citado, e todos os manifests da bateria usam esse hash.
- [x] Cinco documentos de status e tracker §7 atualizados; metadados anteriores ausentes dos status atuais, histórico preservado; git diff --check exit 0.
- [x] Limitações declaradas: amostra finita, perdas por cenário, causalidade parcial, qualidade restrita aos oráculos; ConPTY físico e ZIP não aplicáveis.

## Robustez do harness — 2026-09-15

Endurecimento do medidor, sem mudança de produto e sem nova bateria. Modelo
padrão permanece `gpt-5.6-luna` (high, normal, `openai-codex`); cenários,
prompts, oráculos, ordem alternada e flags dos braços inalterados.

- `run.py`: pré-voo resolve `node`/`pi`/`slim` por PATH e `npm root -g` com erro
  claro antes de criar a campanha; `--version` com timeout. Nome de campanha
  ganha sufixo `-2`, `-3`… em colisão de timestamp (baterias paralelas). Cada
  braço roda sob try/except: falha inesperada grava `{arm}.error.json` com
  traceback e não derruba o outro braço nem a bateria. Oráculo usa o `python`
  do PATH (o mesmo que os agentes invocam), com `oracle_error` registrado em
  vez de crash. Manifesto ganha `fixtures` (sha256/bytes de SPEC, check e
  arquivos do cenário), `harness` (sha256 dos scripts do benchmark, Python,
  plataforma) e `oracle_python`. Workspace temporário é removido após sucesso
  (as cópias `{arm}.workspace/` permanecem na campanha); em falha, fica para
  depuração. `timeout_seconds` (240) e `validation_timeout` (30) viraram
  parâmetros.
- `process_runner.py` (compartilhado): timeout passa a matar a árvore
  (`taskkill /T /F` no Windows) em vez de só o processo direto; falha de
  `Popen` grava `spawn_error` no timing em vez de deixar `timing.json`
  ausente; `pid` registrado. Assinatura e demais campos inalterados.
- `analyze.py`: virou `audit(pi_dir, slim_dir, output)` importável; o runner
  grava `summary.json` na campanha automaticamente quando os dois timings
  existem (falha de auditoria é impressa, não mascara a execução). Output
  default da CLI agora é `<slim_dir>/summary.json` — antes era o diretório do
  script, o que gerou o `summary.json` avulso aqui. Novas verificações:
  manifests do par devem ter mesmo cenário/prompt, arquivos de evidência
  exigidos listados por nome, eventos de ferramenta órfãos tolerados,
  campos de usage ausentes tratados como zero, `timed_out` incluso no gate,
  e todas as asserções com contexto de campanha/turno.
- `daily.py`: qualquer exceção num cenário vira registro `error_type` em
  `failures` e a bateria continua os pares planejados (antes só `SystemExit`
  era capturado); `--timeout` repassa ao runner; `--no-audit` desativa a
  auditoria automática.
- `report.py`: passe único por braço (antes computava duas vezes); braço sem
  `timing.json` ou sem registros auditáveis entra em `problems` em vez de
  sumir em silêncio; nova seção "Pareamento" com razões Slim/Pi por par
  aprovado — mediana de tokens e de wall, vitórias e agregado por cenário,
  que antes eram calculados à mão para este relatório. Diagnóstico para
  iterar o harness: Resumo ganha taxa de acerto de cache e arquivos
  modificados/criados; cenário ganha dispersão (min/mediana/max da razão);
  campanha ganha coluna de arquivos tocados; nova seção "Por ferramenta"
  (calls/falhas/ms por nome); turnos lado a lado por par no JSON
  (`paired.pairs[].turns`); falhas agregadas por causa
  (`failure_causes`: timeout/processo/oráculo/fixtures/não-executado);
  `--baseline <relatorio.json>` compara medianas e vitórias com a bateria
  anterior. `workspace_diff` compara `{arm}.workspace/` com os fixtures
  originais — sha256 do manifesto, ou o registry `daily`/`holdouts` para
  campanhas antigas. JSON ganha `paired`, `by_tool` e `failure_causes`.
- `pi-audit.ts`: observador nunca quebra o braço medido — sem
  `LUNA_AUDIT_FILE` ou com erro de escrita/payload, registra nada em vez de
  lançar exceção no handler. O auditor detecta a ausência pela contagem de
  requests/messages.

Verificação desta etapa (sem chamadas ao provider): `py_compile` em todos os
scripts e `node --check` no observador; `process_runner` exercitado com
executável inexistente (`spawn_error` gravado, `exit_code` nulo) e com
timeout real (árvore morta, sem processo residual); campanhas sintéticas
percorreram `analyze.audit` e `report.py` incluindo par com gate reprovado
excluído do pareamento; `run.main` executou ponta a ponta com
`run_process`/`resolve`/`version` stubados — sucesso gravou `summary.json` e
limpou o workspace, falha de braço gravou validação/erro e propagou
`SystemExit`. `economy-next/scenarios.py` (`daily.SCENARIOS`) e
`run_cycle.py` (`process_runner.main`) continuam compatíveis. Nenhum
benchmark live foi reexecutado; os números históricos acima permanecem como
registrados. `cargo test`/deploy: não aplicável, nenhum código do produto
alterado.

## Bateria live de validação do harness — 2026-09-15 (~19:35–19:46 UTC)

Primeira execução real após o endurecimento: `run.py` solo (`merge_ranges`) +
`daily.py --rounds 1` (6 cenários), modelo `gpt-5.6-luna` high/normal via
`openai-codex`, Pi `0.85.1`, Slim `0.1.0` (sha256 nos manifestos). 7 pares
planejados, 7 executados, 0 falhas de transporte; oráculo externo PASS e
fixtures intactos nos 14 braços. Relatório: `RELATORIO-SLIM-X-PI-2026-09-15.md`
+ `relatorio-2026-09-15.json`.

**Resultado desta amostra:** Slim mais caro em tokens — 108.612 vs 83.300
(+30,4%), mediana da razão Slim/Pi 1,19; vitórias Slim em 2/7 pares
(`js_pagination` −12,4%, `ledger_audit` −15,1%). Pior em `json_cli` (+97,3%)
e `config_migration` (+69,2%). Wall: Slim 238,8s vs Pi 213,3s (+11,9%);
chamadas 32 vs 35; falhas de ferramenta 1 vs 3; arquivos tocados idênticos
(8 mod + 4 novos cada). Diverge das baterias históricas acima — amostra de
1 rodada por cenário, sem teste de significância; evidência de que o
resultado depende do cenário e da rodada, não de direção fixa.

**Diagnóstico novo já útil:** o input por turno do Slim é consistentemente
maior mesmo com `history_bytes` menor — o custo está no envelope por request
(system+schema) e no cache: taxa de acerto 15,7% vs 26,7% do Pi, com
`cache_read` zero na campanha inaugural. Ferramentas com nomes distintos por
braço (Slim `shell`/`patch`/`list`; Pi `bash`/`edit`), o que inviabiliza
comparar chamada-a-chamada; `shell` concentra 22,7s do tempo de ferramenta
Slim (oráculo `check.py`). A única falha Slim foi `shell-syntax`.

**Bug de harness encontrado pela própria execução:** a sessão Slim atual
grava `schema_version 2` (records `entry`/`fact`/`operation`), sem eventos
`ContextSnapshot` — o auditor legado reportava "0 snapshots vs 4 turns" e a
auditoria automática falhava sem mascarar a execução (comportamento correto:
evidência preservada, `audit_failed` impresso). `analyze.py` agora entende os
dois formatos: turnos = entries `assistant` alinhados 1:1 com
`usage.requests`, outcomes via facts `tool.v1`; o assert de thinking
DeepSeek usa `reasoning_tokens` quando não há eventos legados. Em
`report.py`, o turno das tool calls no formato v2 passou a contar entries
`assistant` (correto quando um turno não tem tools) e `duration_ms` por
ferramenta vem do fact `tool.v1` (antes zerado).

Checklist RULES §4 desta etapa:

- [x] Comandos executados: `run.py`, `daily.py --rounds 1`, `analyze.py`,
      `report.py` — saídas registradas acima e nos artefatos das campanhas.
- [x] Números contados nesta execução: 7 pares, 14 braços aprovados,
      32+35 chamadas, tokens e tempos conforme o relatório gerado.
- [x] Evidência preservada: 7 diretórios de campanha com manifest, timings,
      validações, sessões, workspaces e `summary.json` por campanha.
- [x] `cargo test`/deploy: não aplicável — nenhum código de produto alterado;
      mudanças confinadas a `bench/`.
- [x] Limitações: amostra de 1 rodada/cenário; caches de servidor e
      contenção do host não controlados; sem teste de significância;
      instrumentação assimétrica permanece.
