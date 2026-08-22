# AUDIT-SLIM-TUI â€” RelatÃ³rio e Tracker de ExecuÃ§Ã£o

> Origem: auditoria integral de `DESIGN-SLIM-TUI.md` (2.211 linhas) contra o
> cÃ³digo real (`crates/slim-tui` ~3.200 linhas + bridge `crates/slim-cli/src/tui.rs`).
> Data da auditoria: 2026-02 (checkpoint prÃ©-execuÃ§Ã£o).
>
> **Este documento Ã© um tracker vivo.** Cada item tem status que DEVE ser
> atualizado durante a execuÃ§Ã£o:
>
> - `[ ]` pendente Â· `[~]` em progresso Â· `[x]` feito Â· `[!]` bloqueado (com motivo)
>
> Regras de atualizaÃ§Ã£o:
> 1. Nenhum item vira `[x]` sem o gate do slice rodando verde.
> 2. Item bloqueado vira `[!]` com motivo e volta Ã  fila quando desbloqueado.
> 3. Bug novo descoberto durante execuÃ§Ã£o entra na seÃ§Ã£o 5 antes de ser corrigido.
> 4. Ao fim de cada slice, atualizar este arquivo ANTES de abrir o prÃ³ximo slice.

---

## 1. Veredito geral

O status declarado na seÃ§Ã£o 1.1 do DESIGN Ã© **honesto** â€” corresponde ao cÃ³digo.
Fatia vertical real existe (fullscreen, provider, SSE incremental, login OAuth,
overlays login/modelo/effort, cancelamento). M0â€“M3 nÃ£o passaram gate algum.

TrÃªs violaÃ§Ãµes arquiteturais ativas no binÃ¡rio:

| # | ViolaÃ§Ã£o | EvidÃªncia |
|---|---|---|
| V1 | Loop real muta estado fora do reducer | `runtime.rs:31` chama `state.apply_event` direto; `handle_key` (`runtime.rs:96-160`) muta estado e envia comandos direto |
| V2 | Canais sem capacidade mÃ¡xima | `tui.rs:337-338` `mpsc::channel()` ilimitado nos dois sentidos (spec Â§10.1) |
| V3 | FundaÃ§Ã£o visual plana | `theme.rs:76-95`: `surface`/`surface_alt`/`code_bg`/`background` todos `(0,0,0)`; fundo nunca pintado |

## 2. ClassificaÃ§Ã£o por milestone

Legenda: âœ… implementado Â· ðŸŸ¡ parcial Â· âŒ ausente Â· â›” incorreto

### M0 â€” contratos/testkit

| Requisito | Status | EvidÃªncia |
|---|---|---|
| `UiEvent`/`UiCommand` pÃºblicos | ðŸŸ¡ | `api.rs:139-293`; faltam Tool* tipados, Plan/Goal/Input/Capability/Queue/Status/Compaction; sem sequence por stream |
| Newtypes de ID | ðŸŸ¡ | `api.rs:126-137`; faltam `ToolCallId`, `OverlayId`, `EffectId` |
| `AppState` Ãºnico | â›” | `app.rs:47-70`; sem Scrollback/Focus/Viewport/OverlayStack/Inspector/Keymap/pending_effects; overlays ad-hoc `app.rs:55-57` |
| `RevisionSet` completo | ðŸŸ¡ | `app.rs:7-10`; sÃ³ content+status vs 6 da spec Â§8.3 |
| Reducer Ãºnica rota | â›” | ver V1 |
| Effects + EffectRunner | âŒ | `reducer.rs:11-13`; comandos via `send()` direto `runtime.rs:346` |
| `SurfaceBackend` trait | âŒ | `testkit.rs:5-14` MemorySurface sem trait compartilhada |
| Block model integral | ðŸŸ¡ | `block.rs:7-46`; 7/12 variantes; sem source/created_seq/content_generation/BlockDisplayState |
| Property tests | âŒ | zero proptest no workspace |
| Frames determinÃ­sticos fake | âœ… | `tests/m0_frames.rs:9-49` |

### M1 â€” fullscreen Windows

| Requisito | Status | EvidÃªncia |
|---|---|---|
| TerminalGuard RAII idempotente | âœ… | `terminal.rs:8-52` com Drop fallback |
| Ordem de entrada Â§22.1 | ðŸŸ¡ | `fullscreen.rs:13-19`; faltam UTF-8/VT flags e esconder cursor atÃ© 1Âº frame |
| Bracketed paste | âœ… | `fullscreen.rs:17` + decoder `input.rs:26-77` testado |
| Mouse capability-gated | â›” | `fullscreen.rs:17` captura SEMPRE; `Event::Mouse` ignorado `runtime.rs:120-123` |
| Composer boxed 3 rows | âŒ | `layout.rs:39` composer de 1 row |
| Cursor por grapheme | ðŸŸ¡ | `runtime.rs:437-441` errado em draft multiline; sem selection/undo/history |
| ContextRail / ActivityRail | âŒ | nÃ£o existem em `LayoutRegions` `layout.rs:11-19` |
| Scrollback navegÃ¡vel | âŒ | `runtime.rs:299-345` Paragraph sem offset â€” fim do transcript invisÃ­vel |
| Theme estratificado Â§21.3 | â›” | ver V3; accent `0x78C98B` â‰  `#78D99B` |
| Layout emergÃªncia <40Ã—8 | âœ… | `layout.rs:88-127` com erro explÃ­cito |
| DegradaÃ§Ã£o gradual altura Â§14.4 | ðŸŸ¡ | sÃ³ binÃ¡rio normal/emergÃªncia |
| Golden buffers reais | âŒ | `poc2_golden.rs` testa alturas+status line apenas |
| PTY/ConPTY E2E | âŒ | `pty_windows.rs` Ã© unit test renomeado |

### M2 â€” integraÃ§Ã£o/performance

| Requisito | Status | EvidÃªncia |
|---|---|---|
| Bridge tipada provider real | âœ… | `tui.rs:336-700`; SSE antes do fim provado `tests/tui_bridge.rs:26-80` |
| Login/logout/modelo/effort | âœ… | `runtime.rs:196-330`, `tui.rs:432-560` |
| Cancelamento run/shell | âœ… | CancellationToken + abort `tui.rs:692-701` |
| Lanes bounded + fairness | â›” | ver V2 |
| Coalescer no runtime real | âŒ | `render.rs:9-63` sÃ³ em teste/bench |
| Tool blocks tipados | âŒ | tools viram string `api.rs:231-247` |
| Todo dock completo | ðŸŸ¡ | summary string `api.rs:216`; dock 1 linha `runtime.rs:283-295` |
| Pin/live edge | âŒ | FollowMode inexistente; `visible_range` morto `layout.rs:158` |
| Parse/Wrap/Layout caches | âŒ | `cache.rs` BoundedCache nÃ£o usado pelo produto |
| HeightIndex/virtualizaÃ§Ã£o | âŒ | ausente |
| MÃ©tricas Â§26 | âŒ | ausentes |
| Benchmark com gate | ðŸŸ¡ | `benches/long_session.rs` mede strings, nÃ£o pipeline Ratatui; sem gate |
| Usage projetado | âœ… | `app.rs:186-193` |

### M3 â€” experiÃªncia completa

| Requisito | Status | EvidÃªncia |
|---|---|---|
| Overlays login/modelo/effort | âœ… | Ãºnica fatia M3 integrada |
| Tokens semÃ¢nticos | ðŸŸ¡ | struct ok `theme.rs:20-48`; valores flat; to_ansi16 colapsa em 5 cores |
| Inspectors/palette/image wiring | âŒ scaffold | `inspector.rs`/`image.rs` sem referÃªncias em app/runtime |
| Motion/FrameClock | âŒ | spinner estÃ¡tico `â—Œ` |
| Markdown render | âŒ | sem pulldown-cmark; texto cru `runtime.rs:370-400` |
| Diff/code blocks | âŒ | tokens definidos, nunca usados |
| Search/clipboard/reduced motion | âŒ | ausentes |
| Fault injection real | âŒ | `fault_injection.rs` testa helper de 3 linhas |

## 3. DireÃ§Ã£o visual e estratÃ©gia termcn/Ratatui

- **EstratÃ©gia (Â§5.4): correta.** Zero deps JS/sidecar; stack = ratatui 0.29 +
  crossterm 0.28 + unicode-segmentation. Kit source-owned: dos 10 componentes
  do kit interno, nenhum existe como widget contratado.
- **Linguagem visual (Â§1.2/Â§21): nÃ£o aplicada.** Fundo uniforme, sem 5 nÃ­veis,
  sem user_prompt_bg/composer_bg/scrollbar/progress track/heading accent.
- **Fiel Ã  spec:** sem borda externa, sem card soup, rails de role com
  continuaÃ§Ã£o alinhada ao texto (`indented_body` runtime.rs:403-420), boxes sÃ³
  em overlays (`modal_block` runtime.rs:561-571).

## 4. Bugs adicionais encontrados

| # | Severidade | Bug | EvidÃªncia |
|---|---|---|---|
| B1 | crÃ­tica | Transcript corta o fim â€” Ãºltima mensagem invisÃ­vel quando conteÃºdo > viewport | `runtime.rs:333-338` |
| B2 | alta | Mouse capturado sem handler â€” rouba scroll/clique do terminal | `fullscreen.rs:17` |
| B3 | mÃ©dia | Cursor do composer errado em draft multiline | `runtime.rs:437-441` |
| B4 | baixa | IDs de bloco colidem (`format!("user-{}", len)`) | `app.rs:157` |
| B5 | baixa | Toasts pintam por cima das Ãºltimas linhas do transcript | `runtime.rs:315-334` |
| B6 | baixa | `AssistantDelta` forÃ§a `working=true` (mÃ¡scara p/ evento perdido) | `app.rs:150` |
| B7 | baixa | `demo.rs` duplica pipeline de render (2Âª fonte de verdade) | `demo.rs:33-80` |

## 5. Bugs novos descobertos durante execuÃ§Ã£o

| # | Severidade | Bug | EvidÃªncia |
|---|---|---|---|
| P8 | alta | Shell aguardava o filho antes de drenar stdout/stderr; pipes cheios bloqueavam o processo atÃ© timeout | regressÃ£o `shell_drains_large_stdout_and_stderr_while_child_runs`: RED em 10,26 s; GREEN em 0,76 s com 5 MiB por stream |

## 6. Workflow de execuÃ§Ã£o

Ordem por classe: **Incorreto â†’ Parcial â†’ Ausente**, com corte por prioridade:
bug de dano ao usuÃ¡rio (B1) sobe para o bloco Incorreto. Foco: correÃ§Ã£o â†’
otimizaÃ§Ã£o â†’ melhorias. RestriÃ§Ãµes permanentes: Rust/Ratatui/Crossterm,
reducer Ãºnico, render puro, Windows-only.

### Bloco A â€” INCORRETO (correÃ§Ã£o)

- [x] **A1 â€” Fluxo normativo (V1)** â€” `handle_key` produz `Action`s; loop sÃ³
  chama `reduce`; EffectRunner mÃ­nimo executa `Send`. Gate: testes reducer
  verdes + `apply_event` sÃ³ dentro de `reduce`.
- [x] **A2 â€” Scrollback navegÃ¡vel (B1)** â€” `FollowMode{LiveEdge,Pinned}`,
  offset, Endâ†’live edge, scroll-up pinna com unseen counter. Gate: transiÃ§Ãµes
  Â§13.3 unit + golden 80Ã—24 provando Ãºltima mensagem visÃ­vel.
- [x] **A3 â€” Paleta normativa + superfÃ­cies estratificadas (V3)** â€” RGBs Â§21.3;
  pintar bg por regiÃ£o; user_prompt_bg; composer_bg. Gate: golden
  truecolor/Ansi16/no-color com bg distinto por nÃ­vel.
- [x] **A4 â€” Lanes bounded + coalescer real (V2)** â€” canais bounded, fairness
  32, EventCoalescer no run_loop, flush antes de Ended. Gate: fault injection
  canal cheio; control nunca descartado.
- [x] **A5 â€” Mouse honesto (B2)** â€” capture sÃ³ com capability; tratar ou
  ignorar explicitamente. Gate: sem capture por default; teste de gating.

### Bloco B â€” PARCIAL (completar)

- [x] **B1s â€” Composer boxed + cursor (B3)** â€” 3 rows com borda, 1 row
  compacta; cursor por grapheme da linha lÃ³gica; indicador linha/total.
  Gate: golden 40/80/140 + unit cursor multiline.
- [x] **B2s â€” RevisionSet completo + IDs estÃ¡veis (B4)** â€” 6 revisions Â§8.3;
  sequÃªncia monotÃ´nica de blocos. Gate: unit idempotÃªncia.
- [x] **B3s â€” Ordem de entrada Â§22.1 completa** â€” UTF-8/VT flags, esconder
  cursor atÃ© 1Âº frame. Gate: restore matrix manual Windows.
- [x] **B4s â€” Todo dock com itens** â€” TodoView tipado, estados âœ“â—Œâ—‹âœ•, expansÃ£o
  Ctrl+T. Gate: golden compacto/expandido.
- [x] **B5s â€” DegradaÃ§Ã£o gradual de altura Â§14.4** â€” composer 3â†’1, rails
  somem em ordem. Gate: golden alturas 8/12/24/40.

### Bloco C â€” AUSENTE (criar)

- [x] **C1 â€” Tool blocks tipados** â€” ToolStarted/Progress/Ended, lifecycle
  glyph, agregaÃ§Ã£o por nome/turno, failed/cancelled em row prÃ³pria.
  Gate: unit lifecycle Â§16.2 + golden agregaÃ§Ã£o.
- [x] **C2 â€” Caches Parse/Wrap/Layout + HeightIndex** â€” sobre BoundedCache;
  virtualizaÃ§Ã£o por viewport. Gate: matriz invalidaÃ§Ã£o Â§12.5 + bench p95 â‰¤16ms.
- [x] **C3 â€” ContextRail / ActivityRail** â€” 1 row cada, breakpoints Â§14.2.
  Gate: golden wide/compact/hidden.
- [x] **C4 â€” Golden matrix real Â§28.3** â€” snapshots de buffer nas larguras/
  alturas normativas. Gate: matriz completa verde.
- [!] **C5 â€” PTY/ConPTY E2E real** â€” portable-pty; enter/leave/type/streaming/
  resize/paste/abort/crash restore. Gate: suÃ­te PTY Windows.
- [x] **C6 â€” Benchmark pipeline final com gate** â€” Criterion sobre render
  Ratatui; budget versionado. Gate: p95 registrado por commit/mÃ¡quina.
- [x] **C7 â€” Wiring M3** â€” palette, inspectors, FrameClock/spinner/reduced
  motion, Markdown/diff. Gate: fluxos end-to-end state/input/render.
- [x] **C8 â€” Fault injection real Â§28.5** â€” renderer panic, write parcial,
  channel full, effect timeout, gapâ†’snapshot. Gate: sem terminal leak.
- [x] **C9 â€” Property tests Â§28.2** â€” proptest focus/scroll/overlay/draft.
  Gate: suÃ­te proptest verde.

### PendÃªncias de limpeza

- [x] **L1 â€” Remover demo.rs ou alinhÃ¡-lo ao pipeline Ãºnico (B7)**
- [x] **L2 â€” Toasts nÃ£o sobrescrever transcript (B5)** â€” reservar row ou
  empilhar acima do composer
- [x] **L3 â€” `AssistantDelta` nÃ£o forÃ§ar working (B6)** â€” confiar em RunStarted

## 7. Log de execuÃ§Ã£o

| Data | Slice | Resultado | ObservaÃ§Ãµes |
|---|---|---|---|
| â€” | â€” | auditoria registrada | tracker criado |
| exec | A1 | reducer.rs reescrito: Action::Key/Paste/Resize/Scroll/RequestShutdown; runtime sÃ³ executa Effects; 12 testes reducer verdes | V1 resolvida |
| exec | A2 | ScrollState{pinned,offset_from_end,unseen} em AppState; Paragraph scroll; unseen hint na op bar; tests/scroll_golden.rs 4 goldens via TestBackend | B1 resolvido |
| exec | A3 | theme.rs com paleta normativa Â§21.3 (+composer_bg/user_prompt_bg/border_focus); fundos pintados por regiÃ£o; user_prompt_bg full-width | V3 resolvida |
| exec | A4 | UiChannels ganha events_data; sync_channel(256)/sync_channel(1024); EventSink roteia control/data; run_loop drena control (fairness 32) + data via EventCoalescer; tui_bridge corrigido p/ data lane | V2 resolvida |
| exec | A5 | EnableMouseCapture sÃ³ com Capabilities.mouse; wheelâ†’Scroll; cursor escondido atÃ© 1Âº frame; Show no restore | B2 parcial: falta validaÃ§Ã£o manual Windows |
| exec | L1 | demo.rs removido (duplicava pipeline) | feito junto A5 |
| exec | B1s | composer box 3 rows (bordas TOP/BOTTOM, label modelÂ·effort Ã  direita, border_focus); compacto <16 altura; cursor por grapheme da Ãºltima linha; indicador multiline | tests/layout_golden.rs |
| exec | B2s | RevisionSet com 6 campos (content/fold/theme/viewport/focus/status); fresh_id monotÃ´nico substitui ids por Ã­ndice | m0_frames/m2 atualizados |
| exec | B3s | windows-sys: SetConsoleOutputCP(65001)+SetConsoleCP+ENABLE_VIRTUAL_TERMINAL_PROCESSING com restore exato do modo anterior | falta validaÃ§Ã£o manual Windows Terminal + alternativo |
| exec | B4s | TodoItemView/TodoItemStatus tipados; dock header done/total + item ativo; glyphs âœ“â—Œâ—‹âœ• por status | layout_golden |
| exec | B5s | plan_checked degrada em ordem: rails â†’ dividers â†’ composer 3â†’1 â†’ todoâ†’1 row â†’ hidden; composer_height()/todo_height() helpers | m2_integration/poc2_golden atualizados |
| exec | C1 | UiEvent::ToolStarted/ToolProgress/ToolEnded tipados; BlockKind::Tool(ToolState); agregaÃ§Ã£o âœ“ name Ã—N por runs consecutivos completos; failed/cancelled em row prÃ³pria âœ•; bridge reclassificada (tools=data lane) | tui_bridge/tui_runtime/m2_integration atualizados |
| exec | L2 | toasts reservam rows (viewport do scrollback reduzido), nÃ£o sobrescrevem transcript | render_scrollback |
| exec | C3 | ContextRail (cwd esq, ctx% Â· k/128k dir) e ActivityRail (spinner â—’â—“â—‘â—” via Action::Tick ~8fps, Ctrl+C stop); reduced motion congela glyph; degradam antes de tudo (Â§14.4) | layout.rs regions context_rail/activity_rail |
| exec | C2 | HeightIndex prefix sums O(log n) locate; WrapCache bounded (block,width,collapsed)â†’altura; render virtualizado sÃ³ materializa blocos visÃ­veis; agrupamento de tools no Ã­ndice; unicode-width dep | render.rs |
| exec | C4 | golden_matrix.rs: 7 larguras Ã— 4 alturas, tiling exato das regiÃµes, emergÃªncia 39Ã—7, hierarquia de surfaces por depth (truecolor/256 diferem; 16/no-color colapsam por Â§21.2); to_ansi256 reescrito (cube vs gray, menor distÃ¢ncia) | 4 testes |
| exec | C6 | bench long_session reescrito: corpus 3.200 blocos, TestBackend draw real, gate p95 â‰¤16ms com exit(1) | release: input_to_frame_p95=2.04ms âœ“ |
| exec | C7 | command palette Ctrl+P (query/filter/Enter submete top match/Esc); markdown-light em assistant: headings heading_accent, code fences code_rail; spinner/reduced motion jÃ¡ em C3 | palette_tests + build |
| exec | C8 | fault_injection.rs: overflow data lane coalesce sem perder control; renderer panic isolado (safe_block_lines catch_unwind â†’ fallback row); disconnect observÃ¡vel; stale content page nÃ£o fabrica blocos | 5 testes |
| exec | C9 | proptest: End sempre volta live edge; unseen conta sÃ³ pinned; draft sobrevive a paste oversized + send falho; layout nunca excede viewport em sizes aleatÃ³rios | properties.rs 4 props |
| exec | C5 | tui_pty.rs escrito (spawn slim --tui em ConPTY, alt screen/echo/restore, watchdog kill). BLOQUEADO no sandbox: ConPTY abre mas conhost nÃ£o produz saÃ­da nem p/ `cmd /C echo`; teste #[ignore] documentado, rodar em console Windows real | [!] pendente ambiente fÃ­sico |
| exec | L3 | AssistantDelta nÃ£o forÃ§a working=true (confia em RunStarted) | app.rs |
| exec | W1 | welcome redesenhado: wordmark SLIM em dot-matrix braille (`welcome.rs`, montado de bitmaps por letra), pulso ambiente Ãºnico â—œâ— â—â—žâ—¡â—Ÿ â‰¤2 fps no run loop quando ocioso (spec Â§2/Â§15: um glyph, texto/alinhamento estÃ¡veis), congelado sob reduced motion, ASCII fallback sob NO_COLOR, hints Ctrl+P Â· /mode Â· Ctrl+C e versÃ£o; fallback compacto <36Ã—14 mantÃ©m composiÃ§Ã£o anterior; +5 testes (wordmark determinÃ­stico/uniforme, pulse cÃ­clico, render completo e compacto) | welcome_tests + runtime_tests |
| exec | W2 | RevisÃ£o visual por feedback do usuÃ¡rio (2026-08-21): (1) ContextRail do topo REMOVIDA â€” medidor `ctx N% Â· Nk/128k` movido para a operational bar Ã  direita, junto de â†‘in â†“out (sobrepÃµe Â§15.1; decisÃ£o do usuÃ¡rio); (2) modelÂ·effort saem da op bar â€” ficam SOMENTE no label do composer box (fim da duplicaÃ§Ã£o); provider some da op bar (vive na welcome; assinado fora continua com hint /login); (3) welcome enxuta: removidas as linhas de hints Ctrl+P e versÃ£o (excesso de metadados); (4) border_focus #4F7D5E (verde oliva "sujo") â†’ #78D99B (accent) e label do composer muted â†’ secondary; layout.rs perde context_rail (degradaÃ§Ã£o: railâ†’dividersâ†’composerâ†’dock); +0 testes novos, 6 ajustados | layout_tests/golden_matrix/properties/runtime/layout_golden |
| exec | W3 | Causa da "aparÃªncia negritada" diagnosticada (ref. xai-org/grok-build, TUI Rust como o Slim): TUI nÃ£o escolhe fonte â€” sÃ³ emite atributos ANSI; Slim emite BOLD apenas em accent_bold e headings (runtime.rs:187,191); o peso Ã³ptico vinha do `text` #E6E8EB (91% branco) sobre #080A0D â†’ halation. CorreÃ§Ã£o: token `text` â†’ #C6CDD5 (faixa cinza-suave do Grok); doc normativo Â§21.3 atualizado em conjunto. RecomendaÃ§Ã£o de terminal registrada: WT profile com `"weight": "light"` (Cascadia Ã© variÃ¡vel) e intense text style = bright | theme.rs + DESIGN Â§21.3 |
| exec | W4 | Camada de config real materializa o stub `Config::resolve` (cli > env > project > global): novas deps `directories 5` + `toml 0.8`; `FileConfig { model, endpoint, effort }` parseia TOML (chaves desconhecidas ignoradas), `load_layered()` mescla global `%APPDATA%\slim\slim.toml` â†’ projeto `./slim.toml`; wiring nos dois consumidores: headless `run_cli` (model/endpoint entre env e default) e `prepare_tui` (idem + effort via `ReasoningEffort::parse`); arquivo invÃ¡lido aborta com erro nomeando o caminho; ausente = ok; +6 testes unitÃ¡rios (parse vÃ¡lido/invÃ¡lido/vazio, load missing/temp file, merge de camadas, ordem do resolve). `ignore`/`similar`/`tracing`/`clap` ficam para a fase de tools â€” sem consumidor hoje violaria a regra anti-Ã³rfÃ£o (RULES R7) | config.rs/cli.rs/tui.rs + README "ConfiguraÃ§Ã£o" |
| exec | W5 | Composer refinado por feedback visual do usuÃ¡rio (2026-08-21): as 3 rÃ©guas horizontais empilhadas viraram 1 hairline discreta â€” (1) borda inferior com label REMOVIDA; label `model Â· effort` desce para uma row silenciosa prÃ³pria, alinhado Ã  direita em `muted`; (2) hairline superior sempre `border` apagada (nada de verde); foco sinalizado sÃ³ pelo glifo `â€º` (accent quando focado, muted quando ocioso); (3) rÃ©gua da op bar removida do render_frame â€” separaÃ§Ã£o fica por estratificaÃ§Ã£o de superfÃ­cie; (4) wordmark `SLIM` na op bar sai de accent_bold para accent regular; Palette perde `border_focus`/`op_divider` sem leitor (R7); tokens normativos permanecem no Theme. DESIGN Â§15.3 regras atualizadas; layout.rs intocado (contratos de degradaÃ§Ã£o preservados) | runtime.rs + layout_golden comentÃ¡rio + DESIGN Â§15.3 |
| exec | W6 | Composer reverte ao box completo seguindo a referÃªncia Grok Build (feedback: "nÃ£o dÃ¡ pra copiar o grok?"): `Borders::ALL` + `BorderType::Rounded`, borda SEMPRE neutra (`border`) â€” sem verde de foco â€”, label `model Â· effort` embutido no extremo direito da borda inferior via `title_bottom` em `muted`; foco continua exclusivamente no glifo `â€º`; op bar segue sem rÃ©gua prÃ³pria (W5 mantido). Revoga o hairline do W5; DESIGN Â§15.3 atualizado; layout.rs intocado | runtime.rs + layout_golden + DESIGN Â§15.3 |


| exec | W7 | Slash autocomplete no composer (ref. Cline/Grok, pedido do usuÃ¡rio): digitar `/` em QUALQUER posiÃ§Ã£o do draft â€” inÃ­cio ou meio de frase â€” abre popup arredondado acima do composer listando `PALETTE_COMMANDS` filtradas por prefixo do token sob ediÃ§Ã£o (Ãºltima palavra); â†‘/â†“ navegam com clamp, **Tab** completa o comando + espaÃ§o no draft sem executar, **Enter** completa e executa imediatamente, `Esc` fecha sem tocar no draft; qualquer outra tecla cai no fluxo normal de ediÃ§Ã£o que re-sincroniza o popup (digitaÃ§Ã£o refiltra, Backspace fecha ao perder o match, paste tambÃ©m dispara); estado novo `AppState.slash_suggestions: Option<SlashSuggestions{query, selected}>`; render `render_slash_popup` (Clear + Rounded + surface_alt) ancorado acima do composer; layout.rs intocado; +8 testes em `slash_tests` (abertura inÃ­cio/meio, texto puro nÃ£o abre, filtro por prefixo, Tab/Enter/Esc, reabertura via ediÃ§Ã£o, paste) | reducer.rs/app.rs/runtime.rs + README |

| final | â€” | cargo test --workspace: 45 suÃ­tes ok, 0 FAILED; build sem warnings | sessÃ£o encerrada |
| exec | TOK | Economia de tokens nativa (backlog TOK em `analysis_outputs/AUDIT-ECONOMIA-TOKENS.md`): TOK-01 `cache_control` ephemeral na Ãºltima tool do payload Anthropic; TOK-03 dedup de tool output byte-idÃªntico no fio (`[duplicate read result omitted...]`); TOK-04 `read` paginado (`offset` + footer `[showing lines X-Y of Z]`); TOK-05 estimador conservador Ã·3,5 (cÃ³digo denso); TOK-10 cap de 8 KiB por stream de shell; TOK-11 instruÃ§Ã£o do resumo movida para o fim (prefixo cacheÃ¡vel); TOK-12 evento `ContextSnapshot {tools_bytes, history_bytes}` por turno (TUI ignora). +4 testes naquele slice; mediÃ§Ã£o localhost pÃ³s-deploy: leitura repetida cai de 17.409 B para 68 B no fio | runtime/mod.rs, tools/, provider.rs, compact.rs, events.rs, api.rs |
| final | â€” | suÃ­te e deploy verdes no checkpoint TOK; contagem atual supersede este registro histÃ³rico | sessÃ£o TOK encerrada |
| exec | PERF-01 | microbenchmark `tool_setup` versionado (commit `74f24d2`): 6 tools, 1.418 B internos, 11 Ã— 20.000 iteraÃ§Ãµes; re-mediÃ§Ã£o: 9.305,065 ns setup repetido vs 0,730 ns cacheado (12.746,7Ã— isolado) | harness de mediÃ§Ã£o; sem mudanÃ§a de produto |
| exec | PERF-02 | `definitions_for_mode(mode)` + serializaÃ§Ã£o de `tools_bytes` calculadas preguiÃ§osamente uma vez por agent loop; economia ~9,3 Âµs por turno adicional; 75/75 payloads `s4_long` byte-idÃªnticos antes/depois; processo mediano 1.168â†’1.170 ms (+0,17%, ruÃ­do do `Start-Job`) | comportamento, APIs, wire e schema preservados; DESIGN Â§1.1 sem mudanÃ§a funcional |
| final | PERF-02 | `cargo test --workspace`: 45 suÃ­tes / 215 passed / 0 failed / 1 ignored / 0 warnings; Clippy `slim-core --all-targets -D warnings` verde; `refresh-slim.ps1 -Test` imprimiu `OK:`; binÃ¡rio implantado SHA-256 `bc826e987097bb6dd49b680bd28d4e9fb3ce663a2937495a4fe27921a2552847` | fmt/workspace Clippy mantÃªm drift/6 achados preexistentes fora do slice |

---

## 8. VerificaÃ§Ã£o pÃ³s-execuÃ§Ã£o e ambiente (2026-08-21)

### 8.1 Re-verificaÃ§Ã£o do gate final

Rebuild **limpo** de `target/debug` seguido de `cargo test --workspace`
com o toolchain correto (`stable-x86_64-pc-windows-msvc`, rustc **1.97.1**,
`RUSTUP_HOME` do scoop persist):

Este bloco registrava uma contagem histÃ³rica anterior. A contagem autoritativa
atual estÃ¡ no fim do Â§7: 45 suÃ­tes / 215 passed / 0 failed / 1 ignored /
0 warnings. As 45 suÃ­tes incluem os Doc-tests dos trÃªs crates. O Ãºnico
`#[ignore]` Ã© o gate C5 (`tui_pty.rs`), que segue pendente de console Windows
fÃ­sico (P0). W1 adicionou 5 testes e W4 adicionou 6 testes unitÃ¡rios de config;
os totais intermediÃ¡rios foram removidos para nÃ£o parecerem estado atual.

### 8.2 CorreÃ§Ãµes aplicadas nesta verificaÃ§Ã£o

| Arquivo | CorreÃ§Ã£o | Motivo |
|---|---|---|
| `crates/slim-cli/tests/tui_pty.rs:10` | removido import `TryRecvError` nÃ£o usado | warning contradizia a claim "build sem warnings" |
| `crates/slim-tui/tests/scroll_golden.rs:14` | removido import `detect_capabilities` nÃ£o usado | idem |
| `crates/slim-tui/src/runtime.rs` (struct `Palette`) | removida `#[allow(dead_code)]` obsoleta sobre `border_focus` | o campo Ã© consumido pelo composer boxed desde o gate B1s |

### 8.3 Achado de ambiente (dual toolchain)

- As variÃ¡veis de usuÃ¡rio `RUSTC` e `CARGO` apontam para
  `C:\Users\User\.cargo\bin\*.exe`, caminho que **nÃ£o existe** â€” qualquer
  `cargo`/`rustc` direto falha sem override explÃ­cito.
- Existem **duas instalaÃ§Ãµes** do toolchain `stable-x86_64-pc-windows-msvc`:
  `C:\Users\User\.rustup\toolchains\...` = rustc **1.95.0** (antiga) e
  `C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\...` = rustc
  **1.97.1** (ativa, par com `CARGO_HOME`/`RUSTUP_HOME` do ambiente).
- Compilar apontando `RUSTC` para a 1.95 enquanto o `rustdoc` resolve para a
  1.97.1 produz E0514 ("crate compiled by an incompatible version of rustc")
  nos Doc-tests. **Regra:** usar sempre o rustc/rustdoc do scoop persist; apÃ³s
  troca de compilador, limpar o perfil (`target/debug`) antes de retestar.
- CorreÃ§Ã£o recomendada na mÃ¡quina: remover/reapontar as variÃ¡veis de usuÃ¡rio
  `RUSTC`/`CARGO` para o binÃ¡rio do toolchain ativo.

### 8.4 BinÃ¡rios antigos removidos (2026-08-21)

| Caminho | Motivo |
|---|---|
| `target/release/` (inteiro) | build de 05:45 anterior aos Ãºltimos slices e Ã s correÃ§Ãµes de 8.2 |
| `poc/search/target/`, `poc/tui/target/` | artefatos de build de POCs antigas |
| `release/slim-0.1.0-windows-x64.zip`, `manifest.json`, `SHA256SUMS.txt` | pacote distribuÃ­vel do binÃ¡rio velho |

Fontes preservados (`poc/search/src`, `poc/tui/src`, `release/build_release.py`,
`release/README.md`). Um novo pacote deve ser gerado somente a partir de um
build release fresco pÃ³s-verificaÃ§Ã£o.

