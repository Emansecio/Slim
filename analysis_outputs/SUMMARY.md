# Auditoria da TUI Slim

## Conclusão

Implementação avançou bastante como **fatia vertical utilizável**: binário abre fullscreen real, aceita composer, autentica, troca modelo/modo, conecta provider/agent loop/tools, recebe SSE incremental, usage e cancelamento, e restaura terminal.

Porém, **DESIGN-SLIM-TUI M0–M3 não está concluído**. Código atual implementa núcleo básico de M0/M1 e integração central de M2; grande parte do renderer/runtime planejado em M2 e quase toda experiência M3 ainda é scaffold, helper isolado ou teste sintético sem wiring no app real.

## Estado por milestone

| Milestone | Estado auditado | Implementado | Principal lacuna |
|---|---|---|---|
| M0 — contratos/testkit | **Parcial** | `UiEvent`/`UiCommand`, `AppState`, reducer simples, blocos básicos, `ViewModel`, `MemorySurface` | API reduzida; sem `SurfaceBackend`, effect runner, estado/foco/overlay/scroll/revisions completos; modelo de blocos incompleto |
| M1 — fullscreen Windows | **Parcial, fatia funcional** | `FullscreenBackend`, `TerminalGuard` RAII, alternate screen/raw mode, bracketed paste, composer de uma row, input básico, layout e operational bar | Sem PTY E2E real, scrollback navegável, cursor/editor completo, VT/console flags Windows, focus events, restore/fault matrix completa |
| M2 — integração/performance | **Parcial; bridge central forte** | provider/loop/tools reais via `slim-cli`, SSE incremental, usage, mode, model, cancelamento de provider/tool shell | Coalescer e cache não usados pelo runtime; sem lanes bounded/fairness, `HeightIndex`, scroll pin/live edge, caches Parse/Wrap/Layout, Todo real, tool blocks tipados/agregados, content paging real, métricas/gate de benchmark |
| M3 — experiência completa | **Inicial/scaffold** | tokens de theme, fallback de cor/glyph básico, overlays reais de login/modelo; structs isoladas de inspector/palette/image | Inspectors não renderizados, command palette não ligada, sem diff/activity/tree/diagnostics reais, mouse handling, clipboard, busca, reduced motion, imagens de terminal, approval/input/Goal completos, goldens/fault injection reais |

## Evidência principal

### Integração real confirmada no código

- Entrada normal chama TUI real: `crates/slim-cli/src/main.rs:8-11`.
- App fullscreen real: `crates/slim-tui/src/runtime.rs:22-27`.
- Bridge tipada: `crates/slim-cli/src/tui.rs:268-289`.
- Projeção incremental `SessionEvent -> UiEvent`: `crates/slim-cli/src/tui.rs:693-718`.
- Teste exige primeiro delta antes do provider terminar: `crates/slim-cli/tests/tui_bridge.rs:28`.
- Testes de cancelamento cobrem transporte e shell ativo: `crates/slim-cli/tests/tui_bridge.rs:202`, `:286`.
- Fullscreen/RAII existem: `crates/slim-tui/src/fullscreen.rs:10-31`, `crates/slim-tui/src/terminal.rs:9-51`.
- Renderer real desenha scrollback/composer/bar e overlays de login/modelo: `crates/slim-tui/src/runtime.rs:310-345`, `:355-586`.

### Evidência de implementação parcial

- `AppState` tem só estado reduzido: `crates/slim-tui/src/app.rs:44-63`; `RevisionSet` possui apenas `content/status`: `:8-11`.
- Reducer aceita apenas quatro ações: `crates/slim-tui/src/reducer.rs:4-35`.
- `BlockKind` tem sete variantes simples, sem tool/result/compaction/plan/custom: `crates/slim-tui/src/block.rs:4-12`.
- Runtime não usa reducer/effects; aplica eventos diretamente: `crates/slim-tui/src/runtime.rs:35-40`.
- Scrollback monta todos os blocos desde início e só deixa Ratatui recortar; não há offset/anchor/follow mode: `crates/slim-tui/src/runtime.rs:355-373`.
- `EventCoalescer` existe em `crates/slim-tui/src/render.rs:9`, mas referências externas aparecem só em teste/benchmark; não no loop real.
- `BoundedCache` genérico existe em `crates/slim-tui/src/cache.rs:5`, mas não integra render; não existem `ParseCache`, `WrapCache`, `LayoutCache` ou `HeightIndex`.
- Inspector, palette e imagem são helpers isolados: `crates/slim-tui/src/inspector.rs:10-40`, `crates/slim-tui/src/image.rs:12-21`.
- Dependências de Markdown/highlight/width/snapshot/property/PTY planejadas não constam no crate; `Cargo.toml` lista apenas Crossterm, Ratatui, slim-core e unicode-segmentation: `crates/slim-tui/Cargo.toml:7-10`.

## Qualidade dos testes atuais

Arquivos existem para M0–M3, mas nomes superestimam cobertura:

- `m0_frames.rs`: frame textual determinístico e reducer básico.
- `m2_integration.rs`: helpers isolados de coalescing/cache/layout; não exercita loop fullscreen nem cache/coalescer ligados.
- `m3_golden.rs`: assertions sobre structs/theme/fallback; não gera snapshots de buffer.
- `pty_windows.rs`: composer/decoder/key classification; não abre PTY/ConPTY.
- `fault_injection.rs`: um fallback de closure; não injeta falha de terminal/channel/effect/cache.
- `long_session.rs`: mede `ViewModel::derive`, não pipeline Ratatui real; imprime números sem gate/assert de budget.

## Documentação versus realidade

- `DESIGN-SLIM-TUI.md` agora separa contrato-alvo, checkpoint parcial e redesign visual aprovado; a antiga afirmação de integração pendente foi removida.
- README canônico mantém o status correto: “checkpoint de integração parcial; v1 ainda não concluída”.
- `PLANO-IMPLEMENTACAO.md` descreve “M2 central concluída” e M3 como “componentes”; isso é aceitável somente se “central/componentes” não for confundido com gates completos M2/M3.

## Verificação executada

Com toolchain Windows MSVC explícito do projeto:

```text
cargo.exe test -p slim-tui
cargo.exe test -p slim-cli --test tui_runtime --test tui_bridge
```

Resultado fresco: **exit 0; 35 testes passaram, 0 falharam** — 28 no `slim-tui` e 7 na bridge/runtime TUI do `slim-cli`.

Isso confirma que a fatia implementada está verde; não altera a classificação parcial porque vários requisitos do design não existem no caminho real e, portanto, não são exercitados por esses testes.

## Próximo corte recomendado

1. Tornar transcript navegável: scroll offset + live edge/pinned + render apenas viewport.
2. Ligar coalescing e channels bounded ao loop real.
3. Substituir `BoundedCache` isolado por caches Parse/Wrap/Layout usados no render; depois `HeightIndex`.
4. Materializar tool blocks tipados e lifecycle/cancel/output paging.
5. Só então avançar M3: inspectors reais, search/clipboard/mouse/image.
6. Trocar testes “golden/PTY/fault” sintéticos por buffer snapshots, ConPTY e fault injection reais.

Ordem evita construir M3 sobre scroll/render M2 ainda ausentes.
