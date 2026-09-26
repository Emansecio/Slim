# VALIDAÇÃO DE VIABILIDADE — slim-tui com Ratatui, Crossterm e referência do Grok Build

> **Status de implementação:** auditoria de viabilidade e evidência de POCs,
> não prova física universal da aplicação. Os componentes/testes M0–M3 e a
> integração real de `slim --tui` com provider, loop, tools, auth/config,
> streaming, usage e cancelamento existem. Consulte o [status atual](README.md).

> Navegação: [Índice](README.md) · [Decisões finais](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) · [Paridade Rust](RUST-CLI.md) · [Pesquisa](INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md) · [Design TUI](DESIGN-SLIM-TUI.md)  
>
> Status: **arquitetura fullscreen Windows e integração central implementadas;
> POC-0/1/2 verdes, POC-3 parcial, POC-4/6 preliminares**  
> Data da verificação: 2026-08-20  
> Design validado: [DESIGN-SLIM-TUI.md](DESIGN-SLIM-TUI.md)  
> Referência comparativa: Grok Build no commit `d92c5b0b8582fda358de1f97446aa74af44a464f`  
> Segurança do runtime: **irrestrito por decisão; sem permission gate ou sandbox interno**

> O material de inline/Unix abaixo é referência técnica histórica. O produto
> atual é Windows-only e não tem inline/Unix no roadmap.

## 1. Leitor e ação esperada

Este documento é dirigido ao engenheiro ou agente que planejará e implementará
o `slim-tui`.

Após a leitura, ele deve conseguir:

1. distinguir recursos fornecidos por Ratatui/Crossterm de camadas próprias do
   Slim;
2. saber quais decisões visuais já têm mecanismo técnico comprovado;
3. não atribuir à biblioteca garantias de performance ou compatibilidade que
   pertencem ao aplicativo;
4. executar os POCs obrigatórios antes de iniciar milestones de maior risco;
5. bloquear uma entrega que dependa de capability não detectada.

## 2. Limite da evidência

A verificação usou:

- documentação pública do Ratatui e Crossterm;
- código e testes do Grok Build no commit fixado acima;
- a especificação aprovada do `slim-tui`;
- inspeção do ambiente Windows local.

Não houve compilação Rust, benchmark ou teste PTY do Slim. No ambiente usado na
verificação, `rustc`, `cargo` e `cl.exe` não estavam instalados ou disponíveis no
`PATH`. Portanto:

- **viabilidade arquitetural** está confirmada;
- **compatibilidade local e budgets** ainda precisam ser provados pelos POCs;
- esta auditoria não autoriza declarar performance, IME, clipboard, imagem ou
  inline como validados em produção.

Esta auditoria cobre a viabilidade da TUI. A v1 do produto também exige skills,
MCP e subagentes conforme os gates normativos de `RUST-CLI.md`; esses
subsistemas precisam de planos e testes próprios e não são implicitamente
validados por Ratatui/Crossterm.

## 3. Conclusão executiva

Nenhuma decisão aprovada no design é tecnicamente impossível com
Ratatui/Crossterm.

O resultado não será obtido apenas combinando widgets prontos. A fronteira
correta é:

```text
Ratatui/Crossterm
  ├─ células, estilos, layout, cursor, buffers e eventos
  └─ fullscreen/inline básicos e flush diferencial

Slim
  ├─ reducer, ViewModel, blocos e lifecycle
  ├─ agrupamento de tools e Todo residente
  ├─ composer/paste atômico
  ├─ Markdown, diff, caches e virtualização
  ├─ focus, inspectors, overlays e capabilities
  └─ scheduling, backpressure, recovery, testes e benchmarks
```

Essa separação coincide com o mecanismo do Grok Build: ele usa Ratatui e
Crossterm, mas extrai camadas próprias de render, diff, textarea e inline.

## 4. Baseline comprovado no Grok Build

O snapshot examinado declara:

| Dependência | Versão |
|---|---:|
| Ratatui | `0.29` |
| Crossterm | `0.28` |
| Similar | `2.7` |
| Syntect | `5.3` |
| Unicode Segmentation | `1.12.0` |
| Unicode Width | `0.2` |

No pager, Crossterm habilita `event-stream` e `bracketed-paste`. Ratatui habilita
o backend Crossterm. O pager também depende de crates próprios:

- `xai-grok-pager-render` — primitives e pipeline de render;
- `xai-grok-pager-diff` — construção de hunks;
- `xai-ratatui-textarea` — editor e viewport de texto;
- `xai-ratatui-inline` — committed scrollback, resize e terminal inline.

Isso prova que Ratatui é a infraestrutura, não o produto inteiro.

## 5. Matriz completa de viabilidade

Legenda:

- **Direta:** primitive disponível na biblioteca base.
- **Custom comprovada:** precisa de código do Slim, mas o mecanismo está
  demonstrado pelo Grok Build ou pelas APIs base.
- **Custom + POC:** mecanismo representável, mas política/custo próprios do Slim
  ainda precisam de prova empírica.
- **Capability-gated:** funciona apenas quando terminal/SO oferece suporte.
- **POC obrigatória:** possível, porém não deve entrar no plano sem prova local.

| Contrato aprovado | Classificação | Mecanismo |
|---|---|---|
| fundo `#000000`, texto, rails e divisores | Direta | células e `Style` com foreground/background RGB |
| mensagens alinhadas à esquerda e fluidas | Direta | `Rect` com largura útil; wrap por display width |
| zero rows vazias entre blocos | Direta | cálculo de altura e layout do renderer |
| Todo fixo acima do composer | Custom simples | constraints verticais + `TodoDockState` |
| operational bar abaixo do composer | Direta | row de altura fixa e truncamento por prioridade |
| composer visual de uma row | Custom comprovada | editor lógico + viewport horizontal + cursor físico |
| paste atômico com contagem | Custom comprovada | `PasteSegment` preserva payload; renderer mostra token |
| Enter/Shift+Enter/Ctrl+Enter/Alt+Enter | Capability-gated | eventos normalizados e fallback configurável |
| tools agrupadas por nome e turno | Custom comprovada | projeção do ViewModel; blocos originais preservados |
| running/falha/cancelamento em rows próprias | Custom simples | regra de projeção por lifecycle |
| expansão completa inline do grupo | Custom simples | fold state + reflow do scrollback |
| code block com rail e sem painel | Direta | spans/células + rail neutra |
| diff com background por row | Direta + custom | `Style::bg` + parser de diff |
| destaque intraline mais forte | Custom + POC | spans separados dentro da mesma row |
| hunks, contexto curto e gap marker | Custom comprovada | `similar` + estrutura de hunks |
| Markdown e syntax highlight | Custom comprovada | parser incremental + Syntect |
| CJK, emoji e graphemes | Custom comprovada | Unicode Width + Unicode Segmentation |
| sanitização de ANSI/OSC | Custom obrigatória | parser/allowlist antes de materializar spans |
| scrollbar, seleção e search | Custom comprovada | stateful widgets, hit regions e índices próprios |
| inspectors e overlays | Custom comprovada | composição no Buffer + focus stack |
| fullscreen | Direta + guard próprio | alternate screen, raw mode, cursor e RAII |
| flush apenas de células alteradas | Direta | buffers duplos e diff do `Terminal` |
| streaming com janela de 16 ms | POC obrigatória | scheduler/coalescer do Slim, não do Ratatui |
| 3.000 blocos / 5 MiB | POC obrigatória | virtualização, caches e clipping próprios |
| inline com scrollback nativo | POC de alto risco | viewport/scroll regions ou backend próprio |
| mouse | Capability-gated | mouse capture + hit testing próprio |
| clipboard | Capability-gated | API do SO ou OSC 52; fallback obrigatório |
| imagens | Capability-gated | protocolo detectado; placeholder obrigatório |
| truecolor/256/16/no-color | Capability-gated | resolução de theme na entrada da surface |
| golden buffers | Direta | Buffer/TestBackend/MemorySurface |
| PTY E2E | Custom comprovada | harness com PTY real e fixtures determinísticas |

## 6. Evidência por subsistema

### 6.1 Layout, largura fluida e theme

Ratatui renderiza widgets em um `Rect` e grava cada grapheme, foreground e
background em células de `Buffer`. Portanto são representáveis:

- rail na primeira coluna;
- conteúdo ocupando toda a largura restante;
- continuation rows alinhadas ao conteúdo;
- Todo, composer e operational bar com heights fixos;
- backgrounds semânticos de diff;
- preto absoluto e fallbacks de cor.

O terminal pode aplicar transparência, perfil de cor ou substituição do preto.
O Slim consegue emitir `RGB(0, 0, 0)`, mas não controla a composição física do
emulador.

### 6.2 Render diferencial e performance

Ratatui mantém buffers anterior/atual e o `Terminal::flush()` envia apenas as
células alteradas. Isso valida o mecanismo de repaint mínimo.

O Grok Build adiciona sobre ele:

1. parse de Markdown;
2. syntax highlight;
3. wrap dependente da largura;
4. `BlockOutput` com metadata por linha;
5. composição de rail/padding;
6. clipping por viewport e scratch buffer;
7. flush diferencial do Ratatui.

Parse, highlight e wrap são cacheados; clipping e composição visível continuam
no hot path. O pipeline do Slim segue a mesma fronteira e deve manter caches
separados por dependência.

### 6.3 Tools agrupadas

O Grok Build possui verb groups para runs de tools não destrutivas. O header é
atualizado em place durante execução e existem testes PTY de
expand/collapse.

O Slim usará uma política mais simples e própria:

- scope de um assistant turn;
- chave igual ao nome da tool;
- somente sucesso entra no grupo agregado;
- running, failed e cancelled permanecem como exception rows;
- detalhes completos continuam nos blocos originais.

Essa política não exige suporte especial do Ratatui.

### 6.4 Composer e paste atômico

Ratatui não fornece o composer aprovado. O estado e a edição pertencem ao Slim.
Ratatui é responsável apenas por pintar a row e posicionar o cursor.

O Grok Build confirma o padrão com uma textarea própria e teste PTY em que um
paste multiline vira chip compacto, mas o payload completo é enviado ao modelo.
O Slim pode representar o mesmo mecanismo como:

```text
TextElement::Text(...)
TextElement::Paste(PasteId)
```

O payload vive em store próprio; a row mostra apenas
`[Pasted Content N chars]`. Delete/undo operam sobre o elemento, não sobre uma
cópia truncada.

### 6.5 Code e diff

Ratatui permite aplicar background por célula ou span. O Grok Build já renderiza
insert/delete com background semântico, números de linha, syntax highlight,
hunk separators e gap markers.

O destaque intraline mais forte do Slim é adicional. Deve obedecer:

- executar somente para pares delete/insert relacionáveis;
- limitar o tamanho das linhas analisadas;
- abandonar intraline e manter somente row tint quando o limite for excedido;
- nunca atrasar o primeiro frame de um diff longo.

Limites iniciais recomendados para o POC, não ainda como API pública:

- máximo 4 KiB por linha para intraline;
- máximo 200 pares por diff visível;
- fallback imediato para row tint.

Os números devem ser confirmados por benchmark antes de entrarem no contrato
normativo.

### 6.6 Todo residente

O Grok Build possui um `TodoPane` stateful, com limites de altura e estado de
scroll/seleção. Na referência ele é um pane acionável, não o dock fixo aprovado
para o Slim.

**Inferência técnica:** o dock do Slim é mais simples de compor porque usa uma
região permanente do layout e não precisa de z-order. Ratatui não impõe qualquer
restrição que impeça as duas rows compactas ou a expansão in-place.

### 6.7 Fullscreen, input e Windows

Crossterm fornece raw mode, alternate screen, cursor, resize, key, mouse, focus e
bracketed paste. O Slim ainda precisa de `TerminalGuard` próprio e restore
idempotente.

O protocolo Kitty keyboard não está disponível em todos os terminais e não deve
ser requisito. Por isso o design mantém:

- Shift+Enter quando distinguível;
- Ctrl+Enter como fallback Windows;
- key normalization antes do reducer;
- matrix real de Windows Terminal e pelo menos um terminal alternativo.

IME, modifier keys e paste não podem ser considerados validados apenas porque
Crossterm expõe o evento.

### 6.8 Inline

Ratatui suporta viewport inline. Na série 0.29, `scrolling-regions` foi criado
para reduzir flicker em `insert_before`.

O Grok Build mantém `xai-ratatui-inline`, com terminal, segmentação, resize e
emissão ao scrollback próprios. Isso demonstra viabilidade e também confirma que
inline robusto não deve ser tratado como troca de enum sem testes.

O M4 do Slim deve continuar separado do fullscreen. Nenhum código de bookkeeping
de cursor pode ser compartilhado implicitamente entre backends.

### 6.9 Clipboard, mouse e imagens

Esses recursos são possíveis, mas nunca universais:

- mouse exige captura e pode conflitar com seleção nativa;
- clipboard pode usar API do SO ou OSC 52;
- imagens dependem de Kitty/iTerm2/Sixel e geometria do terminal;
- inline pode degradar imagem para placeholder;
- todos precisam de capability detection e fallback textual.

## 7. Decisão de versões

O Grok Build examinado prova o par Ratatui `0.29` + Crossterm `0.28`.

Para o Slim:

1. o primeiro POC comparável deve usar esse par para reduzir variáveis;
2. o POC deve habilitar `event-stream` e `bracketed-paste` no Crossterm;
3. o POC inline deve testar `scrolling-regions` no Ratatui 0.29;
4. `unstable-widget-ref`, usado pelo Grok, não é obrigatório para o design do
   Slim e não deve ser habilitado sem necessidade concreta;
5. migrar para uma versão Ratatui/Crossterm mais nova exige repetir golden,
   resize, input, restore e inline antes de alterar o baseline.

A versão de produção deve ser decidida após o POC, não por copiar o número do
Grok ou escolher automaticamente a release mais recente.

## 8. Dependências técnicas mínimas previstas

### Core M0/M1

- Ratatui;
- Crossterm com bracketed paste e event stream;
- runtime async do Slim;
- Unicode Width;
- Unicode Segmentation.

### M2/M3

- parser Markdown incremental;
- Syntect ou highlighter equivalente;
- Similar ou algoritmo equivalente para hunks;
- sanitizer ANSI/OSC;
- clipboard adapter capability-gated;
- image adapter capability-gated.

### Desenvolvimento

- test backend/memory surface;
- snapshot/golden harness;
- PTY portátil;
- criterion ou harness equivalente;
- tracing sem conteúdo sensível.

Não importar crates internos do Grok Build diretamente sem auditoria de licença,
NOTICE, manutenção e acoplamento. A implementação do Slim permanece original.

## 9. POCs obrigatórios

### POC-0 — toolchain Windows

Objetivo: provar que o ambiente compila Rust MSVC.

Gate:

- `rustc`, `cargo` e linker disponíveis;
- crate vazio compila em debug e release;
- dependências escolhidas resolvem sem conflito.

### POC-1 — lifecycle fullscreen

Objetivo: provar entrada/saída segura no Windows.

Gate:

- entrar/sair 100 vezes sem corromper terminal;
- restore após retorno normal, Ctrl+C e panic;
- cursor, raw mode, paste/focus/mouse e alternate screen restaurados;
- resize durante streaming fake não causa panic ou área negativa.

### POC-2 — contrato visual aprovado

Objetivo: provar que o Buffer representa o design.

Gate:

- goldens em 40×8, 80×24, 99×12, 100×24, 139×12, 140×40 e 200×40;
- mensagem longa usa toda a largura e alinha continuations;
- Todo, composer e operational bar mantêm posição;
- tools agrupam/reabrem e exception rows ficam visíveis;
- diff preserva row tint, intraline e no-color fallback.

### POC-3 — composer/input

Objetivo: validar comportamento real do editor.

Gate:

- draft multiline em viewport de uma row;
- Shift+Enter, Ctrl+Enter e Alt+Enter na matrix Windows;
- paste longo vira elemento atômico sem auto-submit;
- submit envia primeiro/último caractere e payload completo;
- delete/undo não deixam store órfão;
- graphemes, CJK, emoji e IME não deslocam cursor.

### POC-4 — performance/virtualização

Objetivo: verificar os budgets, não apenas render correto.

Gate:

- corpus fixo de 3.000–3.200 blocos e aproximadamente 5 MiB;
- input-to-frame p95 `≤16 ms` com cache warm;
- resize/full redraw p95 `≤50 ms` no hardware registrado;
- cache eviction não muda output;
- scroll pinned permanece estável durante streaming.

### POC-5 — inline (não planejado)

Este POC é mantido apenas como referência técnica histórica. Não é gate do
produto atual, que é Windows-only fullscreen.

Gate:

- committed scrollback preservado;
- resize repetido sem ghost frames;
- streaming não sobrescreve output anterior;
- fallback de overlay funciona;
- cursor bookkeeping não vaza do fullscreen;
- teste com e sem scrolling regions.

### POC-6 — capabilities

Objetivo: provar degradação segura.

Gate:

- truecolor, 256, 16 e no-color;
- Unicode e ASCII glyph fallback;
- mouse ligado/desligado;
- clipboard disponível/indisponível;
- imagem suportada vira render; imagem não suportada vira placeholder.

## 10. Riscos restantes

| Risco | Nível | Tratamento |
|---|---:|---|
| inline/scrollback/resize | fora do produto atual | não executar sem nova decisão |
| restore Windows/ConPTY | alto | RAII + fault injection + PTY |
| 16 ms com sessão longa | alto | caches, clipping e benchmark obrigatório |
| IME/modificadores | médio | key normalization + matrix real |
| seleção/mouse/clipboard | médio | capability e escape para seleção nativa |
| imagens | médio | adapters, budgets e placeholder |
| intraline em linhas gigantes | médio | caps e fallback row-only |
| layout/paleta/rails | baixo | primitive direta + golden buffers |
| Todo fixo | baixo | região normal do layout |
| agrupamento de tools | baixo | projeção pura e testável |

## 11. Condição para iniciar implementação

O projeto pode avançar para o planejamento de M0/M1 quando:

1. o toolchain Windows estiver disponível;
2. o par de versões inicial estiver registrado;
3. POC-1 e POC-2 tiverem plano executável;
4. budgets permanecerem gates, não promessas já alcançadas;
5. inline continuar fora do produto; imagens e clipboard entram somente após
   os POCs de capabilities.

Não há bloqueio arquitetural. Há bloqueio operacional local de toolchain e há
provas empíricas pendentes por milestone.

## 12. Fontes primárias

- [Ratatui — Terminal, viewports, buffers e flush diferencial](https://docs.rs/ratatui/latest/ratatui/struct.Terminal.html)
- [Ratatui — widgets custom/stateful](https://ratatui.rs/recipes/widgets/)
- [Ratatui 0.29 — scrolling regions para inline](https://ratatui.rs/highlights/v029/#terminal-support-scrolling-regions)
- [Crossterm — eventos, focus, mouse e bracketed paste](https://docs.rs/crossterm/latest/crossterm/event/)
- [Crossterm — cores RGB/ANSI](https://docs.rs/crossterm/latest/crossterm/style/enum.Color.html)
- [Grok Build — dependências do pager](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/Cargo.toml)
- [Grok Build — pipeline e caches](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/benches/bench.md)
- [Grok Build — verb groups](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/src/scrollback/state/verb_group.rs)
- [Grok Build — paste chip preservando payload](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/tests/pty_e2e/paste_bracketed_chip_text_sends_full_payload.rs)
- [Grok Build — Todo pane](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/src/views/todo_pane.rs)
- [Grok Build — diff/edit renderer](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/src/scrollback/blocks/tool/edit.rs)
- [Grok Build — camada inline própria](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-ratatui-inline/src/lib.rs)
- [Grok Build — textarea própria](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-ratatui-textarea/src/lib.rs)
