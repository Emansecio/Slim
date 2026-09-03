# Slim Complete UX Polish Design

**Status:** aprovado pelo pedido explícito de 2026-08-29 para implementar integralmente o relatório visual de 2026-08-28, sem nova rodada de aprovação.

## Objetivo

Elevar a TUI e a saída humana do Slim sem trocar sua identidade: conversa first,
near-black, uma cor de foco, movimento discreto e nenhuma superfície ornamental.
O trabalho fecha as lacunas de código/diff, pickers grandes, edição do composer,
inspectors, busca, clipboard/imagem capability-gated e saída headless humana.

## Decisões fechadas

### 1. Pickers e overlays

- Modelo, command palette e slash completion usam a mesma regra de seleção,
  viewport e marcador ASCII `>`.
- A seleção nunca pode existir fora da janela visível.
- O filtro e o footer ficam fixos; somente resultados rolam.
- O seletor de modelo abre somente o provider atual expandido. Os demais grupos
  começam recolhidos e continuam acessíveis por `Space`.
- Overlays usam até 90% do viewport, com largura máxima específica e fallback
  quase fullscreen abaixo de 60 colunas.
- IDs secundários desaparecem antes do truncamento do nome principal.

### 2. Código e diff

- Fenced code usa `code_bg`, rail neutra de uma célula e padding funcional.
- Não há box completo ao redor de cada bloco.
- Fences `diff`/`patch` e patches reconhecíveis recebem row tint para adição,
  remoção, header e contexto; `NO_COLOR` preserva prefixos e geometria.
- O Diff inspector mostra o patch integral mais recente e mantém scroll próprio.
- Inline code continua cyan, sem fundo de card.

### 3. Composer

- O draft passa a ter cursor grapheme-safe e edição com Left/Right, Home/End,
  Delete e Backspace.
- Paste continua atômico: navegação cruza o segmento como uma unidade e Delete
  ou Backspace na borda remove o segmento inteiro.
- O box cresce de uma para no máximo cinco linhas de conteúdo, sem animação.
- Altura abaixo de 12 rows mantém uma linha de conteúdo; abaixo de 40x8 usa o
  modo de emergência já existente.
- A viewport do editor mantém o cursor visível. Linhas horizontais longas usam
  hint `<`; o indicador é `N lines`, não uma posição fictícia `N/N`.

### 4. Motion e chrome

- A borda do composer é estável. Nenhum pulse ou alternância de bold.
- ActivityRail é o locus animado principal. Tool rows usam glyph estático
  quando a rail já comunica a execução, exceto paralelismo observável.
- Motion permanece time-based em 83 ms; status textual permanece em 1 Hz.
- Welcome, reduced motion e estados concluídos são estáticos.
- Footer é contextual e omite métricas desconhecidas ou zeradas.
- O label do composer omite effort default quando o espaço é curto.

### 5. Inspectors e busca

- `Ctrl+D`: Diff; `Ctrl+J`: Activity; `Ctrl+G`: Session tree/diagnostics;
  `Ctrl+F`: busca no transcript.
- Em >=100 colunas o inspector ocupa drawer à direita; em viewports menores é
  overlay quase fullscreen.
- Inspectors leem somente ViewModels derivados de `AppState`; não executam IO.
- Busca destaca ocorrências, mostra `atual/total` e ancora o bloco selecionado.

### 6. Clipboard e imagens

- Clipboard é uma capability explícita. `Ctrl+Y` copia o bloco selecionado ou
  o último bloco visível e apresenta feedback; indisponibilidade não quebra a TUI.
- Imagens são attachments por path quando o provider/modelo aceita imagem.
- Sem protocolo inline real, a conversa mostra chip/placeholder estável. O Slim
  nunca anuncia preview inline quando apenas o fallback textual foi validado.

### 7. Headless humano

- Texto humano imprime a resposta primeiro.
- Stop/usage/timeline aparecem somente em `--verbose`.
- JSONL e códigos de saída permanecem inalterados.

## Arquitetura

- `composer.rs`: modelo de edição e snapshot visual do cursor.
- `picker.rs`: cálculo puro de viewport, seleção e truncamento compartilhado.
- `markdown.rs`: projeção semântica de code/diff para linhas Ratatui.
- `inspector.rs`: estado e ViewModels de diff/activity/session/diagnostics/search.
- `runtime.rs`: composição e pintura; nenhuma decisão de domínio nova.
- `reducer.rs`: key routing e transições determinísticas.
- `image.rs`: attachment/fallback capability-gated; sem simular protocolo inline.

## Tratamento de erro

- Catálogo ou conteúdo vazio mantém overlay operável e footer explicativo.
- Clipboard indisponível gera toast, não panic.
- Imagem inválida ou modelo incompatível permanece no draft como erro visível e
  não é enviada silenciosamente.
- Renderer de bloco continua protegido por `catch_unwind` e fallback local.

## Verificação

- TDD por slice, observando RED antes de GREEN.
- Goldens em 120x30, 100x30, 80x24, 60x16, 40x8 e 32x10.
- Truecolor, ANSI16, `NO_COLOR` e reduced motion.
- Catálogo de pelo menos 100 modelos; seleção sempre visível.
- Code, diff, search, inspectors, composer multiline, clipboard indisponível e
  image fallback.
- `cargo test --workspace`, benchmark de sessão longa, PTY atual, docs vivas e
  `refresh-slim.ps1 -Test` antes da entrega.

## Fora de escopo

- Animações interpoladas, 30/60 fps, `tachyonfx`, gradientes e logo ASCII.
- Dashboard na welcome.
- Copiar identidade ou assets de Grok/Codex/Pi.
- Implementar um protocolo de imagem falso ou provider multimodal não suportado.

## Auto-revisão

- Sem placeholders ou decisões abertas.
- A mudança mantém a arquitetura `AppState -> reducer -> render`.
- O escopo é amplo, mas cada slice produz comportamento independente e testável.
- A spec diferencia fallback de imagem de suporte inline real.
