# Riscos da implementação TUI atual

## P0 — bloqueiam alegação M0–M3 concluído

### 1. Transcript longo fica inacessível

`render_scrollback` sempre começa no primeiro bloco e não possui scroll offset, anchor ou live edge (`crates/slim-tui/src/runtime.rs:355-373`). Ratatui apenas recorta viewport. Após conversa crescer, mensagens novas podem ficar fora da tela.

**Correção mínima:** estado de scroll + renderização da faixa final/visível; depois `HeightIndex`.

### 2. Performance M2 não está no caminho real

`EventCoalescer` e `BoundedCache` são helpers testados isoladamente, sem uso em `run_loop`. Não existem caches Parse/Wrap/Layout nem virtualização. Benchmark mede projeção textual, não frame Ratatui real.

**Impacto:** documentação/testes podem sugerir budget atendido sem medir renderer usado pelo produto.

### 3. Channels não cumprem contrato bounded

Bridge usa `std::sync::mpsc::channel()` sem limite (`crates/slim-cli/src/tui.rs:289-297`); design exige lanes bounded, prioridade e fairness. Stream rápido ou UI lenta pode acumular memória.

**Correção mínima:** channels bounded separados para control/data ou boundary bounded equivalente com política explícita de overflow/coalescing.

## P1 — gaps funcionais importantes

### 4. Tools não possuem modelo visual normativo

Eventos viram `Activity(String)` e notifications. Não há `ToolCallBlock`/`ToolResultBlock`, agregação por nome/turno, expansão, cancelamento focado ou paging real. `RequestContentPage` responde “unavailable” (`crates/slim-cli/src/tui.rs:628-631`).

### 5. M3 existe principalmente como scaffold

`InspectorState`, `CommandPalette` e `render_image` não entram no runtime real. Não há Diff/Activity/SessionTree/Diagnostics renderizados, command palette ligada, search, clipboard, mouse event handling ou imagens de terminal.

### 6. Composer é editor mínimo

Sem cursor interno, selection, preferred column, undo/redo, history ou viewport lógico multiline. Backspace remove último `TextElement`; texto restaurado/inserido em lote pode ser apagado inteiro. Cursor usa contagem de graphemes, não display width de wide chars (`crates/slim-tui/src/runtime.rs:506-511`).

### 7. Terminal Windows incompleto

RAII cobre raw/alternate screen, mas não captura/restaura UTF-8/VT console flags, não habilita focus events, não esconde/mostra cursor conforme lifecycle e ativa mouse sempre, ignorando capability/config. Ordem de restore também diverge da especificação.

### 8. Sanitização/Markdown ausentes

Crate não depende de `pulldown-cmark`, `syntect` ou `unicode-width`. Conteúdo de model/tool não passa pelo pipeline Markdown/ANSI control-safe descrito. Tool output é reduzido a notification textual.

## P2 — confiança de validação

### 9. Testes têm nomes mais fortes que cobertura

- “golden” não usa snapshot de buffer;
- “PTY Windows” não cria PTY/ConPTY;
- fault injection cobre uma closure fallback;
- long-session não mede pipeline real nem falha por budget.

**Risco:** regressões visuais, restore real e desempenho passam despercebidos.

### 10. Status pode ser superestimado fora do design

`DESIGN-SLIM-TUI.md` agora registra checkpoint parcial e redesign visual aprovado. A tabela do plano de execução ainda pode ser lida como M2/M3 concluídos quando somente integração central/helpers existem.

## Heurísticas, não bugs confirmados

- Poll de 16 ms pode atrasar primeiro delta até uma janela, contrariando “imediato”; medir antes de classificar como regressão.
- Acúmulo em channel unbounded depende da taxa real de eventos/UI; arquitetura permite crescimento, mas impacto requer stress test.
- Wide chars podem posicionar cursor errado porque grapheme count não equivale a cell width; confirmar com CJK/emoji em terminal físico.
