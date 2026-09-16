# Slim × Pi: diagnóstico após repetição do benchmark

## Conclusão

O Slim tem uma desvantagem consistente de tokens nesta amostra, apesar de precisar de menos chamadas e apresentar menos falhas de ferramenta. O principal sinal é o volume de entrada repetido em cada chamada. A lentidão generalizada e o déficit de cache sugeridos pela amostra inicial não se repetiram.

A próxima intervenção nativa mais bem justificada é um experimento pequeno na apresentação das ferramentas ao modelo. Ainda não há um A/B de uma mudança no Slim que comprove a economia possível. Esta investigação alterou o medidor, preservou o código de produto preexistente e mediu os mesmos executáveis instalados.

## 1. O que foi efetivamente executado

- **Luna:** quatro rodadas de oito cenários, 32 pares / 64 execuções. Todos passaram pelo verificador externo, com `SPEC.md` e `check.py` intactos e métricas completas.
- **DeepSeek:** duas rodadas de quatro cenários, oito pares tentados / 16 execuções. Todos receberam limite mensal do OpenCode Go; nenhum par utilizável para desempenho. Não houve alteração de saldo, credenciais ou configuração de conta.
- Os seis cenários de `daily.py` foram seguidos pelos dois de `holdouts.py`. A ordem dos cenários foi sorteada com seed `20260915`; em cada cenário Luna, Slim começou duas vezes e Pi duas vezes. As execuções foram sequenciais.
- Luna: `openai-codex / gpt-5.6-luna / high / normal`. Controle bloqueado: `opencode-go / deepseek-v4-flash / high`.
- Igualdade da conta Codex e da chave configurada do OpenCode Go conferida localmente, sem registrar credenciais ou identificadores no relatório.
- Comparação dos defaults nativos: Slim com configuração vazia e Pi sem extensões pessoais, skills, templates ou arquivos de contexto. A extensão Pi apenas observa eventos e payloads. Não representa todas as personalizações possíveis dos usuários.

Os 32 manifestos Luna registram a mesma versão do medidor durante a execução e os mesmos hashes de binários:

| Programa | Versão | SHA-256 |
|---|---|---|
| Slim instalado | 0.1.0 | `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925` |
| Pi instalado | 0.85.1 | `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c` |

O checkout contém WIP de produto preexistente. Os resultados pertencem ao executável instalado identificado acima; não validam automaticamente esse WIP. Depois das campanhas, houve ajustes de redação/indentação do medidor e cobertura adicional de formas de resultados de ferramentas. Essas formas adicionais não mudam a classificação dos payloads efetivamente usados nesta bateria.

## 2. Resultado agregado com Luna

| Métrica | Slim | Pi | Leitura |
|---|---:|---:|---|
| Execuções aprovadas | 32/32 | 32/32 | Empate nos verificadores utilizados |
| Tokens totais | 487.483 | 427.062 | Slim +14,1% |
| Tokens de entrada, incluindo cache | 458.450 | 398.858 | Slim +14,9% |
| Tokens de saída, incluindo raciocínio | 29.033 | 28.204 | Slim +2,9% |
| Chamadas ao modelo | 143 | 173 | Slim −30 chamadas |
| Ferramentas executadas | 217 | 232 | Slim −15 execuções |
| Falhas de ferramenta recuperadas | 2 | 11 | Slim teve menos falhas |
| Tempo total dos processos | 988,964 s | 1.297,845 s | Slim −23,8% |
| Taxa de cache da entrada | 34,5% | 31,1% | Slim +3,4 pontos percentuais |
| Arquivos modificados/criados | 24 / 12 | 24 / 12 | Mesma quantidade, somada por execução |

O Slim venceu **6/32 pares em tokens** e **21/32 em tempo**. A mediana das razões Slim/Pi foi **1,1457 em tokens** e **0,9070 em tempo**: +14,6% e −9,3%, respectivamente. A mediana é calculada sobre as razões de cada par, não pela divisão das medianas dos programas.

O tempo total é sensível a esperas excepcionais. Por exemplo, o Pi teve um intervalo de 94,982 s entre pedido e resposta no SQL da rodada 2 e outro de 67,539 s na auditoria de ledger da rodada 4. Esses casos foram mantidos. O medidor não permite separar processamento remoto, rede e fila do servidor nesse intervalo.

## 3. Resultado por cenário

Cada linha contém quatro pares. Delta de tokens compara as somas das quatro rodadas; delta de tempo é a mediana das razões por par. Valores positivos representam maior consumo/duração no Slim.

| Cenário | Delta tokens | Slim vence tokens | Delta tempo mediano | Slim vence tempo |
|---|---:|---:|---:|---:|
| `merge_ranges` | +36,98% | 0/4 | −9,3% | 4/4 |
| `repair_catalog` | +14,19% | 0/4 | −18,2% | 4/4 |
| `json_cli` | +32,73% | 0/4 | +11,5% | 1/4 |
| `js_pagination` | +17,36% | 0/4 | −14,1% | 4/4 |
| `ledger_audit` | +27,43% | 0/4 | −4,3% | 2/4 |
| `config_migration` | +0,57% | 3/4 | −14,5% | 2/4 |
| `repo_wide_repair` | −9,66% | 3/4 | −27,0% | 3/4 |
| `sqlite_balances` | +9,68% | 0/4 | +37,6% | 1/4 |

Por soma, o Slim foi mais econômico em um dos oito cenários. A configuração ficou próxima do empate agregado, mas teve dispersão grande: −54,6%, −0,5%, +66,8% e −4,8% em tokens, nas quatro rodadas. Não convém tratá-la como uma regressão uniforme.

Os dois cenários adicionais exercitam descoberta em um repositório com 80 arquivos de arquivo histórico e correção de agregações SQL com multiplicação de linhas por joins. São fixtures sintéticas já existentes no projeto; o nome `holdouts.py` não significa teste cego nem comprova generalização para repositórios reais inéditos.

## 4. Causas sustentadas pelas evidências

### A. Entrada recorrente: prioridade alta

Dos **60.421 tokens extras** do Slim, **59.592 são de entrada: 98,6%**. A diferença de saída é somente 829 tokens. A explicação principal não é uma resposta final muito longa.

Nos seis cenários principais, a primeira chamada do Slim já consome **853–872 tokens de entrada adicionais**, média de 862,0. Incluindo os dois cenários adicionais, o intervalo é 853–943. Essa diferença existe antes de executar ferramentas ou recuperar erros.

| Componente serializado | Slim | Pi |
|---|---:|---:|
| Sistema | 1.556 bytes | 2.757 bytes |
| Schemas de ferramentas | 8.030 bytes | 2.845 bytes |

Esses valores são estáveis nos registros Luna. Os schemas do Slim ocupam 2,82 vezes o tamanho do Pi. O texto de sistema do Slim é menor. A entrada também inclui contexto inicial do workspace e instruções de canal; portanto, não se pode atribuir todos os tokens adicionais exclusivamente aos schemas. Bytes não são tokens e não fornecem uma porcentagem garantida de economia.

Em `merge_ranges`, ambos fizeram exatamente quatro chamadas em todas as rodadas. Mesmo assim, o Slim consumiu 30,2%–48,4% mais tokens. Isso reforça que reduzir apenas o número de turnos não resolve o problema.

Ponto de intervenção no checkout: [definições nativas das ferramentas](../../crates/slim-core/src/tools/mod.rs). Há instruções extensas e repetidas e formas alternativas anunciadas no schema, como `max_lines`/`lines` e duas representações de patch. Qualquer simplificação precisa preservar os contratos de edição e a capacidade do modelo de usar a ferramenta corretamente.

### B. Escolhas de leitura e crescimento do histórico: prioridade intermediária

No `ledger_audit` da rodada 1, o Slim leu o CSV inteiro e depois executou um script para calcular o resultado; o Pi calculou a partir do arquivo sem primeiro trazer todo o CSV para o contexto. Ambos fizeram cinco chamadas, mas gastaram 19.793 e 10.554 tokens. Os resultados de ferramentas acumulados ao fim da primeira etapa eram 5.693 bytes no Slim e 2.193 no Pi.

Isso mostra uma oportunidade de evitar transportar dados brutos quando a tarefa só exige uma transformação calculável localmente. Não comprova que impor uma política universal de leitura curta ou truncamento seja melhor: outras tarefas precisam do conteúdo completo.

No `json_cli`, a desvantagem persistiu nas quatro rodadas (+11,9% a +50,2%). O patch redundante após PASS observado na amostra original não se repetiu nas três primeiras novas rodadas. Por isso, aquele episódio não explica a desvantagem recorrente.

Rastros: `20260915-205540Z-ledger_audit`, `20260915-205305Z-json_cli`, `20260915-210037Z-json_cli` e `20260915-210521Z-json_cli`, detalhados no relatório Luna.

### C. Recuperação de JSON em vários arquivos: causa real, porém variável

Na configuração da rodada 3, o Slim gastou **38.972 tokens / 10 chamadas**, contra **23.368 / 9** do Pi. O Slim deixou JSON inválido nos dois arquivos. Corrigiu um após o primeiro erro e só descobriu o problema do segundo na validação seguinte. Foram duas falhas de validação recuperadas em sequência.

Na rodada 1, o Pi sofreu o problema inverso: 26.576 tokens / 10 chamadas contra 12.062 / 4 do Slim. Ambos são suscetíveis a erro de edição, e a qualidade do diagnóstico em lote pode reduzir a recuperação.

Rastros: `20260915-210320Z-config_migration` e `20260915-205630Z-config_migration`. O alvo de melhoria é a recuperação após edição inválida, preservando o motivo e os arquivos afetados; não uma classificação genérica de qualquer `exit 1` como erro de shell.

### D. Contexto inicial do workspace: benefício a preservar

No reparo de repositório, Slim fez quatro chamadas em cada rodada; Pi fez cinco, cinco, seis e cinco. O Slim venceu três rodadas em tokens. Na primeira, leu os arquivos relevantes já no primeiro turno, enquanto o Pi começou com descoberta e depois fez as leituras.

O checkout injeta uma lista inicial limitada de caminhos em [runtime/workspace.rs](../../crates/slim-core/src/runtime/workspace.rs), chamada por [runtime/mod.rs](../../crates/slim-core/src/runtime/mod.rs). O comportamento observado é compatível com esse benefício, mas não houve ablação que prove quanto da vantagem vem da lista. Removê-la indiscriminadamente para poupar entrada pode perder uma vantagem já observada.

### E. Validação nativa: lacuna de evidência, sem causalidade demonstrada para tokens

O Slim registrou `validated_completion=false` em todas as 32 execuções, embora o verificador externo tenha aprovado todas. O classificador atual em [tools/execution.rs](../../crates/slim-core/src/tools/execution.rs) reconhece comandos Cargo específicos como validação; os cenários usam `python check.py`.

Há espaço para reconhecer a validação efetiva do projeto de maneira mais geral. Porém, o Slim terminou normalmente essas tarefas; esse campo falso não demonstra, por si só, a causa de turnos ou tokens extras. Não seria correto promover qualquer comando com código zero a prova de conclusão. A evidência precisa apontar a verificação executada e ser invalidada por mudanças posteriores relevantes.

## 5. Hipóteses que perderam prioridade

- **Cache como causa principal:** a desvantagem de 11 pontos da amostra inicial inverteu-se para vantagem de 3,4 pontos. Cache depende também do estado remoto e foi observado, não controlado. Tokens em cache continuam incluídos no total; melhorar cache não elimina automaticamente tokens contabilizados.
- **Mais falhas de ferramenta no Slim:** foram duas contra onze do Pi, com sucesso final igual nos verificadores.
- **Mais turnos no Slim:** foram 143 contra 173. Há potencial de melhorar decisões específicas, mas a contagem global já favorece o Slim.
- **Compactação ou retries como explicação deste conjunto:** os contadores Slim registraram zero retries e zero tokens de compactação. Esta amostra curta não testa sessões longas, compactação ou retomada.
- **Arquivos modificados como indicador de qualidade:** quantidades iguais não garantem qualidade equivalente fora dos casos verificados.

## 6. Próximos experimentos nativos, em ordem

| Ordem | Mudança candidata | Por que testar | Critério de aceitação |
|---|---|---|---|
| 1 | Encurtar descrições e anunciar uma forma canônica por operação nas ferramentas existentes | Custo inicial e recorrente demonstrado em todos os cenários | Menor entrada no fio; manter contratos e sucesso; repetir A/B sem aumento relevante de falhas ou tempo |
| 2 | Melhorar leitura e apresentação de resultados de dados nas ferramentas existentes | CSV inteiro foi carregado e depois recalculado localmente | Reduzir contexto acumulado no ledger sem prejudicar tarefas que exigem leitura integral; medir também os demais cenários |
| 3 | Melhorar diagnóstico e recuperação de edições inválidas em vários arquivos | Duas correções sequenciais de JSON no caso ruim | Informar causas/arquivos de uma vez quando já houver validação pertinente; manter atomicidade e não impor testes caros a cada patch |
| 4 | Reconhecer evidência de validação além de Cargo no runtime compartilhado | 32 aprovações externas não reconhecidas pelo indicador nativo | Identificar verificação real, não aceitar qualquer exit zero e invalidar evidência após edição; medir impacto antes de prometer economia |

Essas candidatas pertencem às ferramentas e ao runtime compartilhado, sem regras específicas para cada fixture nem necessidade de instalar um `AGENTS.md` em cada projeto. O caminho comum favorece alcance entre agentes/providers, mas a eficácia ainda precisa ser medida com cada família de modelo e com projetos distintos.

O primeiro A/B deve mudar apenas a apresentação das ferramentas, manter o executável atual como referência e separar qualidade, tokens, tempo e falhas. Não remover todo o contexto inicial nem as regras de edição que podem estar ajudando o Slim. Mudanças maiores de compactação, cache ou arquitetura não estão justificadas por estes rastros.

Antes de declarar superioridade em todos os cenários, é necessário repetir a candidata, incluir outro modelo com disponibilidade e testar projetos reais novos. Quatro repetições por cenário são evidência exploratória; não sustentam garantia para qualquer projeto ou agente. O segundo modelo permaneceu **não validado**, por limite de conta.

## 7. Correções do benchmark e validação

- Classificação separada de JSON inválido, falha de validação, uso incorreto de Git e sintaxe de shell.
- Turnos das ferramentas Pi ligados ao identificador da chamada, corrigindo deslocamento por horário.
- Hashes das fixtures calculados dos bytes materializados antes dos dois braços, corrigindo falsos modificados causados por LF/CRLF; remoções também registradas.
- Sistema, schemas, histórico e resultados de ferramentas medidos separadamente. O observador Pi conta antes de omitir raciocínio opaco dos registros salvos. Registros Pi antigos continuam com reconstrução parcial de bytes de histórico.
- Uso incompleto e erro de provider impedem entrada no pareamento. O Pi retornou código de processo zero mesmo com erro de quota; o novo gate o rejeitou corretamente.
- Agenda completa emitida antes da execução, seed reproduzível, rodadas registradas e falhas retidas. O lote DeepSeek terminou com as oito falhas registradas; nenhuma foi substituída por uma tentativa escolhida depois.

Verificações desta sessão:

1. `python -B -m unittest discover -s bench/luna-live -p test_measurement.py -v`: **9/9 aprovados**. Inclui um teste do ciclo do runner com processos simulados e um verificador Python local; esses testes não são evidência de execução do modelo.
2. `node --check bench/luna-live/pi-audit.ts`: aprovado.
3. Auditoria dos 32 pares Luna: aprovada; conferidos completude, modelo, esforço, verificadores, balanceamento e hashes constantes.
4. Reprocessamento dos sete pares antigos: aprovados, tokens preservados. O total correto de arquivos é quatro modificados e quatro criados por programa, em vez de oito modificados e quatro criados.
5. `git -c core.whitespace=cr-at-eol diff --check`: aprovado; referências locais do diagnóstico verificadas. Os avisos de normalização CRLF/LF do Git não são falhas do check.

A imagem inicial mostrava sete pares, mas somente seis cenários diferentes: `merge_ranges` apareceu duas vezes. Comparar “cinco categorias de sete” sem essa distinção superestimava a diversidade da amostra.

Build, suíte Rust e deploy: **não aplicáveis ao escopo de alterações no benchmark**. O produto não foi modificado nesta etapa. As alterações do medidor estão no checkout; não houve commit, push ou deploy.

## 8. Evidência completa e reprodução

- [Relatório detalhado Luna: 32 pares](RELATORIO-SLIM-X-PI-2026-09-15-CONTROLADO-LUNA.md)
- [Dados e manifestos Luna em JSON](RELATORIO-SLIM-X-PI-2026-09-15-CONTROLADO-LUNA.json)
- [Controle DeepSeek bloqueado: oito pares retidos](RELATORIO-SLIM-X-PI-2026-09-15-CONTROLADO-DEEPSEEK.md)
- [Dados do controle DeepSeek em JSON](RELATORIO-SLIM-X-PI-2026-09-15-CONTROLADO-DEEPSEEK.json)
- [Relatório original preservado](RELATORIO-SLIM-X-PI-2026-09-15.md)

Os relatórios completos incluem cada campanha, métricas por turno, ferramentas, erros e hashes. Evidência bruta está nas pastas de campanha listadas nos JSONs, ignoradas pelo Git por convenção do projeto. O primeiro par desta bateria é `20260915-205129Z-repair_catalog`; o último Luna é `20260915-213353Z-sqlite_balances`.

```powershell
python -B -u bench/luna-live/daily.py --rounds 4 --shuffle-seed 20260915
python -B -u bench/luna-live/holdouts.py --rounds 4 --shuffle-seed 20260915
```

O controle DeepSeek reutilizou `daily.SCENARIOS` e `holdouts.SCENARIOS`, selecionando, nesta ordem de registro, `json_cli`, `config_migration`, `repo_wide_repair` e `sqlite_balances`; duas rodadas, mesma seed, provider/modelo explícitos. Não repetir enquanto persistir o limite mensal.

Limites materiais: host compartilhado, estado de cache remoto não controlado, latência de provider variável, instrumentação diferente entre CLIs, fixtures sintéticas e um único modelo efetivamente medido. As conclusões de tokens são consistentes nesta amostra; os ganhos de uma correção nativa e a generalização permanecem por demonstrar.
