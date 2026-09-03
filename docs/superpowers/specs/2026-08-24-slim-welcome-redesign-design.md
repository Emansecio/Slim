# Slim welcome redesign — design

**Data:** 2026-08-24  
**Status:** implementado, validado e implantado

## Objetivo

Transformar a tela vazia do Slim em uma abertura de ferramenta técnica de
precisão. A composição deve priorizar somente identidade, conexão e próxima
ação, com verde restrito a sinais funcionais.

## Problema confirmado

A implementação anterior combinava wordmark braille, tagline e um glyph de pulso
ambiente. O wordmark e o pulso concentram verde e ocupam a hierarquia sem
acrescentar estado ou orientação. A tagline descreve o produto, mas não ajuda o
usuário a começar.

## Direção escolhida: eixo central

A welcome usa uma pilha central, sem contêiner ou decoração:

1. `SLIM` em texto neutro e bold;
2. uma linha de conexão;
3. uma única orientação contextual.

Não há wordmark braille, tagline, borda, métrica, versão, ilustração, pulso,
caret ou animação.

## Conteúdo por estado

### Desconectado

```text
SLIM

○  Not connected
Run /login to connect
```

- `SLIM`, glyph e estado usam texto neutro;
- somente `/login` usa o verde de accent;
- o texto não promete um provider específico.

### Conectado

```text
SLIM

●  Connected · OpenAI Codex
Describe a task to begin
```

- somente `●` usa o verde de accent;
- o nome do provider usa o label já exposto pelo estado atual;
- a orientação não duplica atalhos já disponíveis no footer.

## Responsividade

- A pilha fica centralizada horizontal e verticalmente.
- Com `height >= 4`, o render usa nome, uma linha vazia, estado e orientação.
- Com `height == 3`, remove somente a linha vazia.
- Com `height == 2`, usa `SLIM` na primeira linha e combina estado/ação na
  segunda (`○ Not connected · /login`); conectado conserva nome e estado.
- Com `height == 1`, prioriza nome e estado em uma única linha; a orientação
  permanece disponível no footer/composer.
- O texto deve ser truncado por largura de célula, sem wrap acidental, panic ou
  overflow.
- A welcome não introduz borda, métrica ou variante visual exclusiva para
  terminais largos.

## Cor e acessibilidade

- Verde é destaque funcional, nunca superfície ou bloco de texto.
- Truecolor, ANSI256 e ANSI16 preservam a mesma hierarquia.
- Com `NO_COLOR`, glyph e labels mantêm todo o significado; nenhuma distinção
  depende de cor.
- A welcome não agenda ticks. `SLIM_REDUCED_MOTION=1` produz exatamente o mesmo
  conteúdo e layout do modo normal.
- A remoção do pulso elimina redraws ociosos causados apenas pela welcome.

## Escopo técnico

Alterar somente:

- renderer e helpers diretamente responsáveis pela welcome;
- testes focados de conteúdo, cor, compactação e motion;
- contrato normativo e tracker vivo exigidos pelo workspace;
- números documentais somente se a contagem fresca mudar.

Não alterar composer, footer, estado de autenticação, labels dos providers,
paleta global, transcript ou layout das demais telas. Código exclusivo do
wordmark/pulso deve ser removido se ficar sem consumidor.

## Critérios de aceitação

- wordmark braille, tagline e pulso ausentes do render e do código sem uso;
- nome, estado e orientação presentes nos estados conectado e desconectado;
- verde restrito ao dot conectado e ao comando `/login`;
- normal e reduced motion produzem a mesma welcome e zero motion ticks;
- `NO_COLOR` comunica conexão e ação sem depender de cor;
- golden tests cobrem pelo menos `120×30`, `60×16` e `32×10`;
- screenshots reais registram os mesmos três tamanhos, incluindo um cenário
  `NO_COLOR`/reduced motion;
- `cargo test --workspace` passa sem falhas nem warnings;
- `refresh-slim.ps1 -Test` imprime `OK:` e atualiza o binário do PATH.

## Fora de escopo

- redesign do composer ou footer;
- nova marca gráfica;
- localização do restante da TUI;
- métricas, onboarding, tutorial, atalhos adicionais ou animação substituta.

## Evidência de entrega

- RED focado: `welcome_golden` iniciou com 4 falhas contra a welcome anterior;
- GREEN focado: 64 testes de lib, 15 de motion e 4 goldens da welcome;
- gate integral: 67 suítes, 601 passed, 0 failed, 1 ConPTY físico ignored e
  0 compiler warnings;
- `refresh-slim.ps1 -Test`: exit `0`, `OK:` e `slim 0.1.0` implantado;
- release e binário no PATH: 8.723.968 bytes, SHA-256
  `03D408BBA1ACFFE6D31F0A9E657E45FADC144AC7864E73FBA3969436BE237328`;
- screenshots reais do Windows Terminal:
  - [`120×30`, desconectado](../../../analysis_outputs/welcome-redesign/welcome-120x30-disconnected.png);
  - [`60×16`, conectado](../../../analysis_outputs/welcome-redesign/welcome-60x16-connected.png);
  - [`32×10`, `NO_COLOR` + reduced motion](../../../analysis_outputs/welcome-redesign/welcome-32x10-no-color.png).
