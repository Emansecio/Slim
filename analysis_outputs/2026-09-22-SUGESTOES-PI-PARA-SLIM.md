# Sugestões do Pi aplicáveis ao Slim

Data da análise: **2026-09-22**. Registro da revisão de 120 PRs do
[Pi](https://github.com/earendil-works/pi) e três PRs relacionadas, realizada
nesta conversa. Janela de interesse: últimos seis meses.

**Atualização de implementação (2026-09-22): A1–A4 implementadas por solicitação
posterior do usuário.** M1–M6, B1–B3 e R1–R2 permanecem propostas ou riscos a
investigar. As descrições originais abaixo registram o estado encontrado na
análise; o contrato atual está na [referência de CLI/runtime](../docs/reference/CLI-AND-RUNTIME.md).

### Entrega das quatro altas

1. **A1:** `Ctrl+Z` desfaz e `Ctrl+Shift+Z` refaz o rascunho; cursor e colagens
   são preservados. Histórico limitado a 64 estados/8 MiB e descartado ao enviar
   ou limpar. `Ctrl+Y` mantém a cópia existente.
2. **A2:** `Ctrl+←/→` navega e `Ctrl+Backspace/Delete` remove palavras usando a
   segmentação Unicode existente. Colagens continuam indivisíveis.
3. **A3:** `/retry` retoma a execução pausada em memória após esgotamento da
   recuperação automática, sem adicionar mensagem nem repetir ferramentas
   concluídas. Somente falhas recuperáveis sem texto/raciocínio parcial,
   ferramentas emitidas naquela chamada ou shell jobs em execução são elegíveis.
   Cancelamento, `Retry-After` e limite total de turnos continuam vigentes.
   Não oferece retomada após fechar o processo nem garantia contra cobrança remota.
4. **A4:** selecionar modelo/esforço/Fast vale para a sessão. `/model --default`
   salva explicitamente a escolha atual quando a execução está ociosa; a
   precedência de configuração permanece.

São novas funcionalidades e uma mudança deliberada de comportamento, não quatro
bugs confirmados. A validação e a identidade do binário constam no
[registro de entrega](../release/README.md).

## Resumo e classificação

| Prioridade | Novas funcionalidades ou mudanças de comportamento |
|---|---|
| Alta | A1. Desfazer/refazer; A2. Edição por palavras; A3. Retentar falha de conexão; A4. Modelo da sessão separado do padrão |
| Média | M1. Preservar esforço preferido; M2. Expor destino de links; M3. Editor externo; M4. Exportar diagnóstico; M5. Filtrar busca por tipo; M6. Validar configuração explicitamente |
| Baixa | B1. Orientação de retomada ao sair; B2. Argumentos no autocomplete; B3. Rolagem acelerada |

São **13 oportunidades**, sem classificá-las como bugs confirmados. Há ainda
**dois riscos não confirmados por execução**, R1 e R2, separados abaixo.
Prioridade expressa utilidade e ordem sugerida; não é estimativa de esforço
nem severidade de vulnerabilidade. Em particular, A3 exige mais cuidado que
adicionar um comando à interface.

As referências de código apontam para os arquivos do Slim; os símbolos citados
permitem localizar o fluxo mesmo após mudanças de linha. As evidências são da
análise estática de 2026-09-22. Testes descritos como existentes no Pi foram
lidos nas PRs, não executados localmente. Critérios de validação abaixo são
critérios da análise original; os resultados executados estão no registro de entrega.

## Prioridade alta

### A1. Desfazer e refazer a edição do prompt

- **Classificação:** nova funcionalidade.
- **Origem:** [Pi #6182](https://github.com/earendil-works/pi/pull/6182), fechada sem integração. O Pi tinha undo, mas faltava redo. A proposta acrescenta uma pilha de redo e a invalida quando surge uma nova edição.
- **Slim:** [Composer](../crates/slim-tui/src/composer.rs) mantém texto, cursor e blocos de colagem, sem histórico de edição.
- **Menor solução:** histórico limitado de operações ou estados do rascunho, preservando cursor e elementos de colagem. Cada colagem deve ser uma operação atômica. Não incluir desfazer alterações de arquivos do workspace.
- **Testes da PR:** restauração de edições e invalidação de redo após nova edição.
- **Validação necessária no Slim:** digitação, exclusão, Unicode, colagem, restauração do cursor, limite de memória e limpeza do histórico ao enviar o prompt.

### A2. Navegar e apagar palavras

- **Classificação:** nova funcionalidade.
- **Origem:** [Pi #5068](https://github.com/earendil-works/pi/pull/5068), integrada. Campos de entrada e editor discordavam sobre limites de palavras; a correção compartilha a segmentação.
- **Slim:** [reduce_key](../crates/slim-tui/src/reducer.rs) e [Composer](../crates/slim-tui/src/composer.rs) movimentam e removem caracteres individualmente, sem operações equivalentes por palavra.
- **Menor solução:** acrescentar navegação e exclusão por palavra, mantendo blocos de colagem indivisíveis e respeitando os atalhos existentes.
- **Testes da PR:** Unicode/CJK, pontuação, caminhos e exclusão para frente e para trás.
- **Validação necessária no Slim:** mesmas classes de entrada, bordas do rascunho e interação com elementos colados.

### A3. Retentar falha de conexão sem acrescentar uma mensagem artificial

- **Classificação:** nova funcionalidade; recuperação existente deve ser preservada.
- **Origem:** [Pi #9744](https://github.com/earendil-works/pi/pull/9744), fechada sem integração. Após esgotar tentativas de conexão, o usuário precisava enviar “continue”. A proposta expõe `/retry` para a tentativa abandonada.
- **Slim:** a lista e o despacho em [reducer.rs](../crates/slim-tui/src/reducer.rs) não oferecem `/retry`. O [CLI](../crates/slim-cli/src/cli.rs) já exige decisão explícita para trabalho durável pendente; `--recover` não equivale a repetir uma chamada do provedor.
- **Menor solução:** disponibilizar repetição somente da chamada de provedor elegível, mantendo a conversa e os resultados já concluídos. Rejeitar execução ativa, falha não recuperável e efeitos de ferramentas cujo resultado permaneça incerto.
- **Testes da PR:** esgotamento das tentativas, retry automático desabilitado, ausência de tentativa anterior e erro não recuperável.
- **Validação necessária no Slim:** não duplicar mensagens, ferramentas, cobranças por tentativas disparadas duas vezes ou trabalho pendente; manter cancelamento e recuperação explícita.

### A4. Separar modelo da sessão e modelo padrão

- **Classificação:** mudança de comportamento deliberado, não bug confirmado.
- **Origem:** [Pi #8356](https://github.com/earendil-works/pi/pull/8356), integrada. Seleções temporárias gravavam defaults globais e afetavam sessões futuras. A solução torna a persistência explícita.
- **Slim:** os comandos de seleção em [tui.rs](../crates/slim-cli/src/tui.rs) chamam `persist_model`, que grava modelo e esforço globalmente por meio de [save_global_model](../crates/slim-cli/src/config.rs).
- **Menor solução:** distinguir “usar nesta sessão” de “salvar como padrão”, com indicação clara de escopo. A escolha do comportamento padrão precisa constar do contrato antes da implementação.
- **Testes/evidência da PR:** casos de modelo/esforço por sessão e persistência explícita. A [#9902](https://github.com/earendil-works/pi/pull/9902) trata posteriormente a preservação do esforço nas trocas; ver M1.
- **Validação necessária no Slim:** troca temporária não altera o arquivo global; salvar padrão persiste; novas sessões respeitam a precedência de configuração existente.

## Prioridade média

### M1. Preservar o esforço preferido ao trocar modelos

- **Classificação:** melhoria de comportamento.
- **Origem:** [Pi #9902](https://github.com/earendil-works/pi/pull/9902), aberta na análise. Trocas reaplicavam defaults e perdiam a preferência anterior à limitação imposta pelo modelo.
- **Slim:** `catalog_effort`, em [reducer.rs](../crates/slim-tui/src/reducer.rs), conserva o esforço atual quando compatível; caso contrário, escolhe o primeiro nível disponível.
- **Menor solução:** separar esforço desejado de esforço efetivamente enviado e restaurar a preferência ao voltar para modelo compatível. Uma escolha explícita do usuário deve substituir a preferência anterior.
- **Testes da PR:** múltiplas limitações consecutivas e restauração do nível original.
- **Validação necessária no Slim:** sequência de modelos com capacidades diferentes, escolha manual intermediária e independência entre preferência da sessão e padrão global.

### M2. Expor o destino de links no Markdown

- **Classificação:** melhoria de usabilidade; não regressão confirmada.
- **Origem:** [Pi #3248](https://github.com/earendil-works/pi/pull/3248), integrada. Acrescenta hyperlinks OSC 8 e mantém fallback textual para terminais sem suporte.
- **Slim:** [markdown.rs](../crates/slim-tui/src/markdown.rs) considera o estilo do link, mas descarta seu destino na apresentação. O [teste golden](../crates/slim-tui/tests/integration/markdown_golden.rs) exige que a URL não apareça. O texto original permanece disponível para cópia em [reducer.rs](../crates/slim-tui/src/reducer.rs).
- **Menor solução:** permitir consultar o destino ou exibir `rótulo (URL)`, sem duplicar quando rótulo e URL forem iguais. Atualizar o contrato e o teste correspondente; suporte OSC 8 pode ser posterior.
- **Testes e regressões no Pi:** a #3248 testa quebra de linha e fallback. Um ajuste posterior desabilitou hyperlinks em terminais desconhecidos/tmux/screen. A [#7657](https://github.com/earendil-works/pi/pull/7657) fechou hyperlinks truncados; a [#7665](https://github.com/earendil-works/pi/pull/7665) evitou varredura desnecessária de texto comum.
- **Validação necessária no Slim:** largura estreita, truncamento, Unicode, rótulo igual ao destino e preservação da cópia original.

### M3. Editar prompts em editor externo

- **Classificação:** nova funcionalidade.
- **Origem:** [Pi #6123](https://github.com/earendil-works/pi/pull/6123), fechada sem integração. Configuração explícita do editor resolve problemas de propagação de `VISUAL`/`EDITOR`, especialmente no Windows.
- **Slim:** não foi encontrada integração de editor externo no [Composer](../crates/slim-tui/src/composer.rs) nem campo correspondente em [FileConfig](../crates/slim-cli/src/config.rs).
- **Menor solução:** abrir o rascunho em editor configurado, aguardar e importar o resultado. Preservar o rascunho original em falha/cancelamento e definir como blocos de colagem são representados.
- **Testes da PR:** não adiciona testes.
- **Validação necessária no Slim:** caminho com espaços, argumentos do editor, retorno não zero, cancelamento, restauração do terminal e limite de tamanho do rascunho.

### M4. Exportar diagnóstico local

- **Classificação:** nova funcionalidade.
- **Origem:** [Pi #9841](https://github.com/earendil-works/pi/pull/9841), integrada. Um bloqueio offline impedia exportação local; a correção restringe o bloqueio à etapa de upload.
- **Slim:** `/diagnostics`, em [reducer.rs](../crates/slim-tui/src/reducer.rs), abre um painel; não exporta os dados.
- **Menor solução:** exportar localmente os diagnósticos já coletados, com versão, modelo e tempos pertinentes. Remover credenciais e não incluir a conversa por padrão. Não adicionar upload nesta etapa.
- **Testes/evidência da PR:** relata execução da suíte existente de bug report; o diff não adiciona teste específico novo para exportação offline.
- **Validação necessária no Slim:** exportação sem rede, falha de gravação explícita, ausência de segredos/conversa e consistência com os dados exibidos.

### M5. Filtrar busca por tipo de arquivo

- **Classificação:** nova funcionalidade.
- **Origem:** [Pi #2772](https://github.com/earendil-works/pi/pull/2772), fechada sem integração. Acrescenta filtro de tipo, contexto assimétrico, busca multilinha e escape de padrões iniciados por hífen.
- **Slim:** a [busca nativa](../crates/slim-core/src/tools/search.rs) oferece caminho, padrões, contexto e paginação, mas não filtro equivalente por tipo. Seu matching literal nativo não interpreta o padrão como opção de processo.
- **Menor solução:** aproveitar somente filtro por extensão/tipo inicialmente. Incluir o filtro na identidade dos snapshots e cursores para não reutilizar resultados incompatíveis. Preservar limites de leitura, resultados e cobertura.
- **Testes da PR:** filtro de tipo, multilinha, padrões com hífen e contexto assimétrico.
- **Validação necessária no Slim:** exclusão de tipos não selecionados, paginação e cache separados por filtro, resultados vazios e manutenção dos limites. Medir economia de tokens; não assumir percentuais.

### M6. Validação assistida da configuração

- **Classificação:** nova funcionalidade.
- **Origem:** [Pi #9880](https://github.com/earendil-works/pi/pull/9880), aberta na análise. Publica schemas derivados de contratos canônicos, com metadados e testes contra divergência dos artefatos.
- **Slim:** [FileConfig](../crates/slim-cli/src/config.rs) ignora campos desconhecidos deliberadamente para manter compatibilidade; isso também pode esconder erros de digitação.
- **Menor solução:** verificação explícita que avise sobre campos desconhecidos e indique o campo inválido, preservando a carga normal compatível. Schema para editor é uma etapa posterior, não requisito para o primeiro incremento.
- **Testes da PR:** artefatos gerados, campos conhecidos inválidos e compatibilidade com configurações permissivas. A PR registra limitações de execução da suíte completa naquele worktree.
- **Validação necessária no Slim:** erro de digitação, campos futuros preservados, configuração válida e precedência entre camadas sem alteração.

## Prioridade baixa

### B1. Mostrar orientação de retomada ao sair

- **Classificação:** nova funcionalidade pequena.
- **Origem:** [Pi #5176](https://github.com/earendil-works/pi/pull/5176), integrada. Facilita localizar uma sessão após sair da interface.
- **Slim:** [run_tui](../crates/slim-cli/src/tui.rs) encerra a interface e aguarda o worker sem essa orientação.
- **Menor solução:** mostrar identificação e caminho de retomada apenas para sessão persistida e retomável. Usar o fluxo real do Slim: o [CLI](../crates/slim-cli/src/cli.rs) exige prompt explícito para `--resume`, portanto não copiar literalmente o comando do Pi nem emitir um comando inválido.
- **Testes da PR:** caminhos com espaços/aspas, sessão não persistida, saída sem TTY e encerramento por sinal.
- **Validação necessária no Slim:** orientação correta após saída limpa, ausência de indicação para sessão não retomável e tratamento de caminhos Windows.

### B2. Mostrar argumentos no autocomplete

- **Classificação:** nova funcionalidade pequena.
- **Origem:** [Pi #2780](https://github.com/earendil-works/pi/pull/2780), integrada. Exibe argumentos obrigatórios/opcionais antes da execução de templates.
- **Slim:** [reducer.rs](../crates/slim-tui/src/reducer.rs) descreve comandos de forma genérica; `/image` mostra a sintaxe depois da tentativa sem argumento.
- **Menor solução:** acrescentar dicas como `/image <caminho>` aos metadados existentes. Não criar um sistema de templates nem inferir suporte a argumentos nomeados.
- **Testes da PR:** dica obrigatória, opcional, ausente, vazia e caracteres especiais.
- **Validação necessária no Slim:** sugestões estreitas, filtro de comandos e correspondência entre dica e parser existente.

### B3. Rolagem acelerada com Alt

- **Classificação:** nova funcionalidade pequena.
- **Origem:** [Pi #9166](https://github.com/earendil-works/pi/pull/9166), integrada. Multiplica a distância percorrida pela roda quando Alt está pressionado.
- **Slim:** `mouse_action`, em [runtime.rs](../crates/slim-tui/src/runtime.rs), não usa o modificador para acelerar a roda.
- **Menor solução:** multiplicador restrito ao scroll apropriado, mantendo modais e painéis como proprietários da entrada.
- **Testes da PR:** diferença de deslocamento ao usar Alt.
- **Validação necessária no Slim:** roda normal/Alt, limites de rolagem, painel sob o ponteiro e modal aberto sem mover o transcript ao fundo.

## Riscos pendentes de investigação

### R1. Atualizações concorrentes da configuração

- **Prioridade de investigação:** alta, pelo possível impacto em dados de configuração.
- **Classificação:** risco ainda não confirmado por execução; há proteção parcial.
- **Origem:** [Pi #8543](https://github.com/earendil-works/pi/pull/8543), fechada sem integração. Relata sobrescrita por estado desatualizado e escrita não atômica; propõe releitura e substituição atômica. Inclui teste de preservação de alterações externas ao salvar campo diferente.
- **Slim:** [config.rs](../crates/slim-cli/src/config.rs) já relê o arquivo, preserva campos e grava por substituição atômica. `CONFIG_WRITE_LOCK` coordena threads do mesmo processo, não instâncias distintas.
- **Hipótese:** duas instâncias podem ler a mesma versão, alterar campos independentes e a última gravação perder a alteração da primeira. Atomicidade do arquivo não garante atomicidade da transação inteira.
- **Investigação mínima:** teste com dois processos e arquivo temporário isolado, sincronizando a leitura antes das duas gravações. Não usar a configuração real do usuário.
- **Correção somente se confirmado:** coordenação entre processos cobrindo leitura, alteração e gravação, reutilizando infraestrutura existente quando aplicável.
- **Critério:** ambas as alterações independentes preservadas; falha de lock/gravação reportada sem corromper o arquivo.

### R2. Binário cujos bytes são UTF-8 válido aceito como texto

- **Prioridade de investigação:** média.
- **Classificação:** risco ainda não confirmado quanto ao impacto; há proteção parcial.
- **Origem:** [Pi #5288](https://github.com/earendil-works/pi/pull/5288), fechada sem integração e sem novos testes. Propõe detecção de NUL antes de decodificar arquivos não tratados como imagem.
- **Slim:** `read_utf8_line`, em [read.rs](../crates/slim-core/src/tools/read.rs), já rejeita UTF-8 inválido, mas NUL é válido em UTF-8. A [busca](../crates/slim-core/src/tools/search.rs) já verifica NUL em uma amostra.
- **Limite da comparação:** a falha de Postgres relatada na PR não se aplica à persistência JSONL do Slim. Detectar NUL é uma heurística de binário, não uma validação de UTF-8 nem um detector completo de formatos.
- **Investigação mínima:** verificar a saída real de `read` para arquivos representativos e sua apresentação ao modelo.
- **Correção somente se confirmado:** reutilizar detecção limitada para devolver erro útil em vez de conteúdo binário, preservando leitura de texto válido e seus limites.
- **Critério:** arquivo classificado como binário recebe orientação explícita, sem falso sucesso ou alegação de extração do conteúdo.

## Proteções existentes e propostas não adotadas

| Tema | Classificação e evidência |
|---|---|
| `Retry-After` inválido — [#9888](https://github.com/earendil-works/pi/pull/9888) | **Proteção existente:** `parse_retry_after`, em [provider.rs](../crates/slim-core/src/provider.rs), usa parsing restrito e não reduz atraso numérico por overflow. |
| Limites e loops — [#5247](https://github.com/earendil-works/pi/pull/5247), [#9539](https://github.com/earendil-works/pi/pull/9539) | **Proteção existente:** limites e detecção de trabalho repetido no [runtime](../crates/slim-core/src/runtime/mod.rs). Não importar outro loop guard sem demonstrar lacuna. |
| Repetição de efeitos após recuperação | **Proteção existente:** o [CLI](../crates/slim-cli/src/cli.rs) exige decisão explícita para trabalho durável pendente. Preservar essa condição em A3. |
| Compactação para aproveitar cache — [#8307](https://github.com/earendil-works/pi/pull/8307) | **Risco ainda não confirmado como oportunidade no Slim:** a proposta recebeu pedido de reformulação e os commits preparatórios foram revertidos. Não tratá-la como solução validada. |
| Contabilização Anthropic — [#8308](https://github.com/earendil-works/pi/pull/8308), revertida pela [#8313](https://github.com/earendil-works/pi/pull/8313) | **Não aplicável como recomendação pronta:** não copiar uma solução revertida. O revert, sozinho, não prova a causa técnica da regressão. |
| Configuração manual de cache | **Fora das propostas desta rodada:** já dispensada na conversa; não reintroduzir como tarefa aprovada. |

## Cobertura e limites

A seleção principal compreendeu 120 PRs; outras três foram lidas para aprofundar
relações e correções posteriores. No momento da análise, o conjunto de 123 tinha
34 PRs integradas, 79 fechadas sem integração e 10 abertas. Esses estados são um
registro histórico, não uma consulta atualizada automaticamente.

A revisão considerou descrição, discussão, diff e testes disponíveis, com
aprofundamento nos candidatos aplicáveis. Não equivale a auditoria linha a linha
de todos os diffs nem à revisão de todas as PRs dos seis meses. Fechamento
automático por regra de contribuição não significa rejeição técnica.

### Seleção principal: 120 PRs

Os números abaixo identificam PRs em `https://github.com/earendil-works/pi/pull/`.
As referências diretas dos achados estão nas seções anteriores.

```text
9920 9907 9902 9880 9888 9882 9866 9846 9830 9841 9833 9832 9800 9799 9746 9668 9781 9779 9772 9754
9744 9736 9738 9734 9722 9717 9692 9677 9662 9601 8612 6881 9619 9615 9607 9604 8732 9274 9459 9461
9126 9550 9539 9517 9514 9478 9337 9292 9259 9251 9227 9080 9166 8800 8559 8766 8678 8795 8592 8543
8536 8505 8399 8307 8313 8308 8283 8120 8141 6182 6123 6018 5784 5999 5898 5874 5809 5758 5731 5647
5585 5521 5518 5481 5480 5437 5281 5288 5277 5247 5195 5176 5076 5093 5068 3955 3417 3197 3963 3948
3431 3409 3345 3248 3171 3092 2772 2598 2535 2530 2780 2655 2647 2642 2593 2561 2749 2826 3398 4013
```

### Aprofundamento adicional

- [#7657](https://github.com/earendil-works/pi/pull/7657): fechamento de hyperlink truncado.
- [#7665](https://github.com/earendil-works/pi/pull/7665): evitar varredura OSC 8 em texto comum.
- [#8356](https://github.com/earendil-works/pi/pull/8356): separar preferências da sessão e padrões globais.

Não foram executados testes das propostas nem benchmarks de economia de tokens
ou desempenho. A documentação não promete percentuais de economia. Implementação,
testes de código e deploy não fazem parte desta etapa documental.
