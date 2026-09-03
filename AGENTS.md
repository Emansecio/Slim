# AGENTS.md

## Propósito
Esquema mínimo suficiente. Proibido engenharia excessiva: designs e testes que não provam necessidade, por padrão, não fazer.

## Fluxo
1. Entender a demanda antes de agir. Não modificar código primeiro e adivinhar a intenção depois.
2. Raciocínio alto só no planejamento; execução em médio-baixo (ou modelo mais leve).
3. Sem agentes paralelos por padrão. Uma linha de trabalho por vez.
4. Só as skills necessárias; nada de processo pesado.
5. Plano mínimo antes de executar: objetivo, não objetivos, critérios de aceitação, escopo intocado. Reafirmar os quatro antes de agir.

## Pare e simplifique quando
- Abstração, framework, camada de config ou compatibilidade dupla sem necessidade atual; design para futuro hipotético; empilhar restrição sobre restrição.
- Tocar muitos arquivos irrelevantes de uma vez; usar teste novo como desculpa para expandir escopo.
- O modelo de execução começar a empilhar arquitetura: reescreva o plano mínimo.
- Base de julgamento errada invalida qualquer raciocínio: leia o código relevante em vez de montar conclusão com busca (busca não substitui leitura).

## Irreversível
1. Operação irreversível exige a senha do usuário; sem senha (ou errada), recusar sempre.
2. Não irreversível por padrão: rollback/restauração/troca de branch no Git, mover arquivos para backup do repo, rodar testes, ver diff, gerar plano, análise somente leitura.

## Testes
Servem só à aceitação da mudança atual — nunca a cobertura histórica ou sistema de testes futuro.
1. Rode os existentes relacionados primeiro; se provam a mudança, não adicione.
2. Adicione no máximo 1 caminho principal (+1 falha crítica se preciso), só se o comportamento mudou sem cobertura ou o usuário pediu.
3. Proibido: expandir escopo, novos frameworks/infra de teste, snapshots grandes, matrizes parametrizadas, e2e, testes de limites fora da demanda, forçar o produto a acompanhar o teste.
4. Teste mais longo ou complexo que a implementação = engenharia excessiva; delete o teste ou reduza a implementação.

## Entrega
- Diff pequeno: só os arquivos necessários, sem debug residual, sem extra para parecer completo.
- Checklist canônico em `RULES.md` §4 (prevalece sobre este arquivo).

## Regra geral
Confirmar a intenção primeiro, aceitar com modificações mínimas depois.
