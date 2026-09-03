# AGENTS.md

## Propósito
Completar a tarefa atual com o esquema mínimo suficiente.
Proibido engenharia excessiva.
O planejamento pode ser mais robusto, mas a execução deve ser leve.
Designs que não podem provar ser necessários, por padrão, não fazer.
Testes que não podem provar ser necessários, por padrão, não adicionar.

## Fluxo de Trabalho
1. Primeiro entender a demanda, depois agir. Não modifique o código primeiro e depois adivinhe a intenção.
2. Na fase de planejamento, pode-se usar raciocínio mais alto. Na fase de execução, use raciocínio médio-baixo por padrão, ou mude para um modelo mais leve para implementação.
3. Não mantenha o modo de raciocínio mais alto ativado o tempo todo.
4. Não inicie múltiplos Agents em paralelo por padrão. Complete uma tarefa em linha única primeiro, depois decida se precisa dividi-la.
5. Ative apenas as skills necessárias para completar a tarefa. Não instale skills de processos pesados.
6. Primeiro produza o plano mínimo, depois execute. O plano deve especificar claramente:
   - Objetivo
   - Não objetivos
   - Critérios de aceitação
   - Escopo não modificado

## Modos de Falha
1. Não entender verdadeiramente a intenção, apenas corrigir problemas superficiais.
2. Algo que poderia ser resolvido com uma limpeza única de causa raiz, mas em vez disso usar patches históricos, camadas de compatibilidade, implementações duplas, cópias e branches para engordar o código.
3. Projetar excessivamente para casos raros, aumentando o custo de manutenção diária.
4. Base de julgamento errada, não importa quão completo o raciocínio, a conclusão ainda estará errada.
5. Em vez de ler o código diretamente para localizar o problema, usar busca ou adivinhação como substituto para a leitura.
6. Usar "adicionar testes" como desculpa para continuar adicionando abstrações, expandindo o escopo e parecendo completo.

## Limites de Ação
1. Antes de agir, primeiro reafirme:
   - O que o usuário realmente quer
   - Qual é o escopo desta vez
   - Coisas explicitamente não feitas
   - Como considerar concluído
2. Qualquer operação irreversível deve aguardar a confirmação do usuário com a senha antes de executar.
   - A senha de confirmação é especificada pelo usuário.
   - Sem senha, senha errada ou outras respostas, recusar a execução em todos os casos.
3. As seguintes operações não são consideradas irreversíveis por padrão e podem ser executadas:
   - Rollback do Git, restauração, troca de branch
   - Mover arquivos para o diretório de backup do repositório atual
   - Executar testes, visualizar diff, gerar plano, análise somente leitura
4. Ao descobrir que está fazendo as seguintes coisas, deve parar imediatamente e mudar para um esquema menor:
   - Adicionar abstração, framework ou camada de configuração, mas a demanda atual não precisa
   - Projetar antecipadamente para algo que pode ser usado no futuro
   - Continuar empilhando mais restrições para satisfazer restrições
   - Modificar muitos arquivos irrelevantes ao mesmo tempo
   - Criar uma segunda implementação para compatibilizar lógica antiga
   - Usar a oportunidade para adicionar um sistema completo de testes

## Testes
Os testes servem apenas à aceitação das modificações atuais.
Os testes não são responsáveis por completar a cobertura histórica, nem por projetar sistemas de testes futuros.

1. Priorize executar os testes existentes relacionados à modificação atual.
2. Se os testes existentes puderem provar que a modificação está correta, não adicione novos testes.
3. Apenas adicione novos testes nas duas situações abaixo:
   - Desta vez, o comportamento foi alterado, mas os testes existentes não cobrem esse comportamento
   - O usuário explicitamente requer adicionar testes
4. Novos testes cobrem no máximo 1 caminho principal da modificação real desta vez, e, se necessário, adicionam 1 caminho de falha crítica.
5. Proibido expandir o escopo de testes para torná-lo mais completo.
6. Proibido usar a oportunidade para completar testes de módulos irrelevantes.
7. Proibido introduzir novos frameworks de teste, ferramentas de teste ou infraestrutura de teste.
8. Proibido escrever grandes snapshots, matrizes parametrizadas ou suítes end-to-end.
9. Proibido escrever testes para limites não requeridos pela demanda atual.
10. Proibido modificar testes primeiro e depois forçar o comportamento do produto a se tornar mais complexo.
11. Proibido usar "tornar os testes verdes" como razão para continuar adicionando abstrações.

Antes de adicionar qualquer teste, deve ser capaz de responder:
- Este teste está validando qual demanda já aceita
- Se removê-lo, os testes existentes não conseguirão detectar esta regressão
- Ele é mais complexo que a implementação em si

Se o código de teste for mais longo ou mais complicado que o código de implementação, considere por padrão como engenharia excessiva; delete o teste ou reduza a implementação.

## Divisão de Trabalho do Modelo
- Esclarecimento de demandas e revisão de esquemas: use modelos mais fortes
- Escrever código, modificar código, executar testes: use modelos de configuração média-baixa, ou modelos de execução mais leves
- Ao descobrir que o modelo de execução começa a empilhar arquitetura, adicionar compatibilidade, expandir escopo, adicionar grandes conjuntos de testes: pare imediatamente e reescreva o plano mínimo

## Verificação Antes da Conclusão
- Já reafirmou a intenção e os critérios de aceitação
- O esquema é o esquema mínimo, não o máximo
- Já marcou os não objetivos
- Priorizou ler o código relevante, em vez de montar conclusões com busca
- Modificou apenas o conjunto mínimo de arquivos necessários para completar a tarefa
- Testes existentes relevantes já foram executados
- Não adicionou testes para cenários não requeridos
- Se adicionou testes, apenas bloqueou o comportamento desta vez, e em pouca quantidade
- Os testes não introduziram novas dependências ou nova estrutura de diretórios
- Diff pequeno, sem arquivos extras, sem código de depuração residual
- Não construiu extra para parecer completo

## Regra Geral
Primeiro confirme a intenção, depois complete a aceitação com modificações mínimas.
Designs que não podem provar ser necessários, por padrão, não fazer.
Testes que não podem provar ser necessários, por padrão, não adicionar.
