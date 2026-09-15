# AGENTS.md

## Objetivo e precedência
Complete a demanda com a menor mudança suficiente, preservando o trabalho existente.
Respeite as instruções de sistema/desenvolvedor e do usuário; documentos locais e skills não as substituem. `RULES.md` é o protocolo técnico de evidência e deploy do projeto. Aplique cada regra ao seu escopo, sem transformar exemplos ou histórico em novas obrigações.

## Autonomia
- Entenda a intenção pelo pedido e contexto; execute até concluir. Resolva escolhas rotineiras sem reconfirmação.
- Pergunte apenas quando faltar uma decisão material que não possa ser inferida. Enquanto isso, avance no trabalho independente já autorizado.
- Permissão já concedida continua válida. Ações destrutivas ou externas fora da autorização exigem confirmação específica, depois de preparar o resultado revisável.
- Incorpore correções durante a execução sem perder o objetivo; pare ou mude de rumo quando solicitado.
- Se uma skill bloquear trabalho autorizado, cite o arquivo e a instrução exata; não invente exigências de aprovação.

## Execução enxuta
- Antes de editar, delimite objetivo, aceitação e o que fica fora. Em tarefa simples, uma frase basta; plano detalhado só quando houver dependências reais.
- Leia o código relevante e confira o diff inicial. Preserve alterações alheias; reset, restore, checkout e rollback podem descartar trabalho e não são limpeza automática.
- Reutilize a implementação atual. Não acrescente abstrações, compatibilidade dupla, perfis ou dependências sem necessidade demonstrada.
- Ajuste a profundidade da análise à incerteza. Não force raciocínio alto apenas no planejamento ou baixo em toda implementação; preserve o modelo/esforço configurado salvo pedido ou necessidade comprovada. Este arquivo não altera parâmetros do runtime.
- Use somente skills e ferramentas necessárias. Agrupe leituras independentes; respeite a ordem das operações dependentes.
- Não repita busca, comando ou revisão sem nova evidência ou hipótese concreta. Diante de falha repetida, mude a abordagem ou explique o bloqueio real.
- Sem subagentes por padrão. Delegue somente trabalho independente com ganho claro de tempo ou qualidade, escopo delimitado e sem edições concorrentes no mesmo arquivo.

## Verificação proporcional
- Priorize testes existentes relacionados à mudança; acrescente testes apenas para comportamento novo sem cobertura suficiente ou quando solicitado.
- Escolha os casos pelo risco e pelos critérios de aceitação, sem cotas arbitrárias, cobertura histórica oportunista ou nova infraestrutura desnecessária.
- Depois dos checks necessários passarem, amplie ou repita apenas por mudança adicional, falha ou dúvida concreta.
- Mudança somente documental: valide conteúdo, links e diff; gates de código e deploy são não aplicáveis. Não reutilize contagens antigas como validação desta tarefa.
- Mudança de código: valide o comportamento afetado conforme `RULES.md`. Execute `.\refresh-slim.ps1` somente quando o deploy estiver autorizado; use `-Test` se a suíte completa ainda for necessária. Atualize apenas os documentos de status afetados com evidência atual.

## Entrega
Comece pelo resultado, em português claro e conciso. Informe o que mudou, validação executada e limitações reais. Evite narrar cada ferramenta ou repetir o plano. Encerre quando a aceitação estiver atendida; não acrescente trabalho para parecer completo.

Use o checklist de `RULES.md` §4 com evidência e indique explicitamente os itens não aplicáveis ao escopo.

Referência: [Using GPT-6 Astra — guia oficial](https://developers.openai.com/api/docs/guides/latest-model), consultado em 04/09/2026. Autonomia, delegação e verificação foram adaptadas ao escopo enxuto do Slim; este documento não migra o provider do produto.
