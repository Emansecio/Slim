# DESIGN-SLIM-TUI — especificação completa da interface terminal do Slim

> **Status de implementação:** fundação M0–M2 executada conforme os slices do
> tracker (`AUDIT-SLIM-TUI-TRACKER.md`). O binário tem reducer único normativo,
> scrollback virtualizado e navegável, surfaces estratificadas da paleta §21.3,
> lanes bounded com coalescer no caminho real, tool blocks tipados agregados,
> rails operacionais, composer boxed, command palette, motion básico,
> property/fault/golden tests e benchmark com gate. Pendências principais:
> PTY/ConPTY E2E em console físico, caches Parse/Layout dedicados (o WrapCache
> cobre alturas), Todo/Plan/Goal end-to-end pelo harness e a matriz física
> Windows. O detalhamento factual está na seção 1.1.

> Navegação: [Índice](README.md) · [Decisões finais](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) · [Paridade Rust](RUST-CLI.md) · [Pesquisa](INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md) · [Viabilidade](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)  
>
> Status: **design funcional e visual aprovado; grill fechado em 2026-08-20**  
> Data: 2026-08-20  
> Escopo: fullscreen Windows da v1; inline e Unix não planejados  
> Plataforma: Windows-only  
> Superfícies: fullscreen; headless usa o mesmo runtime  
> Completude: **contrato TUI/runtime v1 descrito; fatia vertical integrada;
> gates M0–M3 ainda não concluídos**  
> Base técnica: Ratatui + Crossterm com modelo, caches e renderização controlados pelo Slim  
> Viabilidade: [VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)

## 1. Objetivo deste documento

Este documento especifica o `slim-tui` sem depender do contexto da conversa que
o originou. Um agente de implementação deve conseguir:

1. entender o produto e seus limites;
2. criar os tipos e boundaries corretos;
3. implementar um milestone por vez;
4. saber exatamente o que testar;
5. reconhecer quando uma entrega está completa;
6. evitar decisões arquiteturais já rejeitadas.

O documento descreve o destino completo, mas **não autoriza uma implementação
big-bang**. A ordem dos milestones é normativa.

### 1.1 Checkpoint de implementação atual

Esta seção separa o **contrato-alvo** do que existe hoje. As demais seções
continuam normativas: um tipo, helper ou arquivo presente não significa que o
milestone correspondente passou seu gate.

| Área | Estado atual | Confirmado | Falta para o gate |
|---|---|---|---|
| M0 — contratos/testkit | **Quase completo** | `UiEvent`/`UiCommand` ampliados (Tool* tipados), `AppState` único, reducer como única rota de mutação, effects executados pelo runtime, blocos User/Assistant/Thinking/Tool/System/Error/Activity/QueuedUser, `RevisionSet` de 6 campos, IDs monotônicos, `MemorySurface` + frames determinísticos + proptest | `SurfaceBackend` trait compartilhada, variantes Plan/Compaction/Custom, sequences por stream |
| M1 — fullscreen Windows | **Fatia ampla; gate físico pendente** | TerminalGuard RAII + UTF-8/VT flags com restore exato, alternate screen/raw mode/paste/cursor oculto, mouse capability-gated, composer boxed 3 rows com cursor por grapheme, ContextRail/ActivityRail, scrollback navegável com pin/live-edge/unseen, paleta §21.3 estratificada em truecolor/256/16/no-color, layout de emergência, golden matrix via TestBackend | PTY/ConPTY E2E físico (teste escrito, `#[ignore]`, requer console real) e validação manual Windows Terminal + alternativo |
| M2 — integração/performance | **Integração central + pipeline essencial** | bridge tipada com lanes bounded (control 256/data 1024) e fairness 32, coalescer no runtime real, tool blocks tipados com agregação por nome/turno, HeightIndex prefix sums + WrapCache bounded + render virtualizado, cancelamento provider/shell, usage, modos, modelo/effort; bench long-session com gate p95 ≤16 ms (medido: ~2 ms release) | Todo dock end-to-end via eventos reais do harness, ParseCache/LayoutCache dedicados, métricas §26 expostas |
| M3 — experiência completa | **Parcial** | overlays login/modelo/effort integrados, command palette Ctrl+P funcional, markdown-light (headings/code fences), spinner animado com FrameClock por Tick, reduced motion, toasts que reservam rows, fault injection com catch_unwind por bloco | inspectors diff/activity/tree/diagnostics renderizados, clipboard/search, imagens, diff real, matriz golden completa de estados M3 |

#### O que já funciona end-to-end

- entrada fullscreen normal, inclusive sem credencial;
- `/login`, `/logout` e seleção de modelo/effort para os providers suportados;
- prompt do composer até provider, agent loop e tools reais;
- primeiro delta SSE publicado antes do término da resposta;
- lifecycle básico de run/tool, resposta final e usage projetados para a TUI;
- troca `Auto → Read-only → Plan → Auto` no boundary de modo;
- cancelamento encerra request ativo e processo shell iniciado pelo run;
- shutdown explícito com restore RAII como fallback;
- transcript navegável: live edge segue o fim, setas/PageUp pinnam, End volta,
  contador de unseen na operational bar;
- tool blocks tipados com glyph de lifecycle, preview limitado e agregação
  `✓ nome ×N` para runs concluídos; falha/cancelamento em row própria;
- command palette (Ctrl+P) filtrando slash commands com Enter para executar;
- tela de boas-vindas com wordmark SLIM em dot-matrix braille, pulso ambiente
  de um único glyph a ≤2 fps enquanto ocioso (congelado sob reduced motion,
  fallback ASCII/compacto), status de conexão e um único hint;
- operational bar única na base: SLIM/modo/estado à esquerda, medidor de
  contexto (`ctx N% · Nk/128k`) + tokens ↑↓ à direita — sem rail superior
  (W2, 2026-08-21: substitui a antiga ContextRail) e sem duplicar model/effort,
  que vivem apenas no label do composer box;
- verificação atual: workspace verde (45 suítes / 220 passed / 0 failed /
  1 ConPTY físico ignored), incluindo proptest, fault injection e golden
  matrix via TestBackend; re-verificado por `refresh-slim.ps1 -Test` em
  2026-08-22 (rustc 1.97.1), com build/deploy/smoke aprovados (tracker §7);

#### O que precisa ser corrigido antes de continuar M3

1. **P0 — provar o ciclo físico Windows.** PTY/ConPTY E2E está implementado
   (`crates/slim-cli/tests/tui_pty.rs`) mas ignorado neste ambiente sem console;
   rodar em console físico e validar restore/IME/mouse na matriz §25.
2. **P1 — completar contratos M0.** Sequences monotônicas por stream,
   `SurfaceBackend` trait compartilhada entre MemorySurface e Fullscreen,
   variantes Plan/Compaction/Custom do block model.
3. **P1 — Todo/Plan/Goal end-to-end.** O dock tipado existe; falta o harness
   emitir `TodoChanged`/`PlanChanged`/`GoalChanged` reais e os fluxos de
   aprovação/input dos fluxos Plan/Goal/Input.
4. **P1 — ParseCache e LayoutCache dedicados.** O WrapCache cobre alturas;
   faltam os caches de Markdown parse e LayoutPlan das chaves §12.3.
5. **P2 — inspectors e busca.** Diff/Activity/SessionTree/Diagnostics ainda
   não têm drawer renderizado; search no histórico visível ausente.
6. **P2 — métricas §26.** Contadores de latência/cache/lane existem nos testes
   e no bench, mas não são exportados como telemetry de runtime.
7. **P2 — clipboard/search/imagem.** Capability flags detectados; features não
   ligadas a fluxo real.

#### Regra de atualização deste checkpoint

- **Implementado:** usado pelo binário normal e coberto por teste observável.
- **Parcial:** existe uma fatia real, mas faltam entregas ou gate do milestone.
- **Scaffold:** tipos/helpers existem sem wiring completo no produto.
- **Concluído:** somente após executar o gate normativo inteiro; presença de
  arquivos ou testes unitários isolados não basta.

### 1.2 Direção visual revisada

A referência primária é a linguagem visual do **Universe TUI/Grok Build**:
profundidade por surfaces near-black, prompt elevado, composer contornado,
headings coloridos, metadata discreta, scrollbar, progress rail e motion de
atividade. A referência secundária é a energia operacional do **OpenCode**:
rails mais expressivas, foco forte e progresso evidente, sem copiar seu excesso
cromático ou densidade de status.

Proporção normativa: **80% contenção da primeira referência, 20% energia da
segunda**.

Para componentes individuais, **termcn é a principal referência de polimento**.
Ele não entra como dependência: seus componentes React/OpenTUI são estudados e
portados seletivamente para widgets Rust controlados sobre Ratatui. Tokscale
orienta padrões já comprovados em Ratatui; Critique orienta diff/responsividade;
OpenTUI orienta primitives e keymaps, não o runtime do Slim.

Preservar:

- transcript denso e legível, sem cards em cada mensagem;
- verde Slim como identidade principal;
- azul frio reservado para headings, links e informação estrutural;
- warning/error somente para estado semântico;
- metadata e shortcuts discretos;
- foco visual forte no composer e atividade corrente.

Evitar:

- fundo uniforme sem hierarquia;
- laranja ou verde em toda metadata;
- rails grossas em todos os blocos;
- múltiplas status rows repetindo a mesma informação;
- animação de tela inteira ou motion que dificulte leitura;
- boxes em cada mensagem, canto decorativo ou chrome sem função.

A profundidade vem de cinco níveis controlados: `background`, `surface`,
`surface_alt`, `surface_elevated` e overlays. Motion vem de progress rail,
spinner, streaming caret, tool progress e pulso sutil de foco; nunca de conteúdo
textual se movendo.

## 2. Decisões fechadas

Estas decisões não devem ser reabertas durante a implementação sem uma revisão
explícita deste design:

| Tema | Decisão |
|---|---|
| Framework | Ratatui sobre Crossterm |
| Estratégia | híbrida: infraestrutura externa, modelo/render pipeline próprios |
| Estado | reducer único e effects assíncronos |
| Render | puro; sem I/O nem mutação de estado de aplicação |
| Superfície inicial | fullscreen em alternate screen |
| Segunda superfície | headless, reutilizando o mesmo AppHandle/ViewModel |
| Layout | conversa focada; ContextRail superior; Todo fixo; composer elevado; inspectors adaptativos |
| Tema default | near-black estratificado, verde Slim + azul estrutural, profundidade sem card soup |
| Mensagens | alinhadas à esquerda e fluidas; prompt do usuário pode usar surface elevada |
| Densidade | transcript compacto; composer boxed com três rows normais; rails operacionais com uma row |
| Tools | sucessos agrupados por nome/turno; running/falha/cancelamento em rows próprias |
| Code/diff | code em rail neutra; diff com highlight de linha e intraline |
| Plataforma | Windows-only |
| Unix/inline | não planejados |
| Histórico visual | blocos tipados com IDs e revisions estáveis |
| Streaming | primeiro delta imediato; demais coalescidos em até 16 ms |
| Motion | activity/progress contínuos e contidos; ambient idle até 2 fps; reduced motion zera ambient motion |
| Event delivery | lifecycle lossless; deltas/ticks/resize coalescíveis |
| Caches | parse, wrap e layout separados por dependência |
| Extensões | custom blocks versionados somente após o core estabilizar |
| Crates | `slim-tui` com AppHandle tipado; core fica em `slim-core` |

## 3. O que será construído

### 3.1 Capacidades finais

As capacidades abaixo descrevem o renderer fullscreen da v1. Qualquer menção
posterior a `InlineBackend`, Unix ou parity entre surfaces é referência histórica
e não autorização de implementação.

- chat interativo com streaming;
- scrollback virtualizado;
- composer multiline em box de três rows, com uma row editável e fallback compacto;
- render de Markdown e código;
- blocos de user, assistant, thinking, tool, system, compaction e error;
- tools com progress, output expandível e cancelamento;
- operational bar adaptativa abaixo do composer;
- Todo em dock fixo acima do composer;
- drawers para diff, activity, session tree e diagnóstico;
- overlays/modals empilháveis;
- busca no histórico visível;
- themes semânticos;
- mouse opcional;
- clipboard e bracketed paste;
- reduced motion e fallback de glyphs;
- imagens quando o terminal oferecer protocolo suportado;
- fullscreen Windows;
- custom blocks/widgets somente se necessários ao core, sem plugin system;
- golden tests, PTY E2E, fault injection e benchmarks long-session.

### 3.2 Fora do escopo do `slim-tui`

O crate não implementa:

- provider de LLM;
- agent loop;
- tool execution;
- permissions/sandbox, ausentes do produto Slim por decisão normativa;
- compaction;
- persistência de sessão;
- filesystem/VCS;
- MCP;
- subagentes.

Skills, MCP e subagentes são requisitos do runtime da v1. Estão fora do
`slim-tui` apenas por ownership: chegam como `UiEvent`/ViewModel e aparecem no
transcript, status e activity views; a interface não executa skill, MCP call ou
child agent.

Esses subsistemas publicam eventos e recebem comandos por interfaces tipadas.
A TUI apresenta e coleta interação por um `AppHandle` tipado; ela não se torna a
autoridade de domínio nem executa providers/tools diretamente.

## 4. Princípios de arquitetura

### 4.1 Fluxo unidirecional

```text
TerminalEvent ─┐
AgentEvent ────┤
Tick ──────────┼─> Action ─> reduce(AppState, Action) ─> Effect[]
AsyncResult ───┘                    │                        │
                                    ▼                        ▼
                                ViewModel              EffectRunner
                                    │                        │
                                    ▼                        └─> Action
                               LayoutPlan
                                    │
                                    ▼
                                  Frame
                                    │
                                  │
                                  ▼
                          FullscreenBackend
```

### 4.2 Uma autoridade de estado

`AppState` é a única autoridade do estado visual e interativo. Componentes não
guardam cópias divergentes de session, focus, scroll ou tool status.

Caches podem ter mutabilidade interna, mas:

- derivam somente de dados versionados;
- podem ser descartados a qualquer momento;
- nunca são persistidos como estado de produto;
- um cache miss não muda a semântica do frame.

### 4.3 Render puro

Render recebe `&ViewModel`, `LayoutPlan`, theme e um buffer. Ele não pode:

- executar I/O;
- enviar commands;
- alterar `AppState`;
- aguardar futures;
- chamar relógio global diretamente;
- ler filesystem;
- iniciar timers.

Animações recebem um `FrameClock` já calculado pelo loop.

### 4.4 Effects explícitos

Toda ação externa é um `Effect`, por exemplo:

- enviar prompt;
- abortar run;
- cancelar tool;
- copiar texto;
- abrir link;
- carregar imagem;
- mudar backend;
- salvar setting;
- solicitar snapshot de sessão.

O effect runner executa o trabalho e publica `Action::EffectCompleted` ou
`Action::EffectFailed`.

## 5. Dependências técnicas

O snapshot do Grok Build estudado usa Ratatui 0.29 e Crossterm 0.28. Esses
números são referência de arquitetura, não obrigação de versão. O plano de
implementação deve fixar versões compatíveis atuais e registrar o lockfile.

### 5.1 Runtime

| Capacidade | Dependência pretendida |
|---|---|
| terminal buffer/layout/diff | `ratatui` |
| eventos, raw mode, alternate screen | `crossterm` |
| async/event loop | runtime async do Slim |
| cancelamento | token de cancelamento do runtime |
| Unicode width | `unicode-width` |
| grapheme boundaries | `unicode-segmentation` |
| Markdown | `pulldown-cmark` |
| syntax highlight | `syntect`, inicializado sob demanda |
| motion/transições complexas | `tachyonfx`, opcional e somente após spike |
| imagens capability-gated | `ratatui-image`, somente em M3 |
| session tree | `tui-tree-widget` somente se reduzir código sem criar estado paralelo |
| serialização de themes/API | `serde` |

### 5.2 Desenvolvimento

| Capacidade | Ferramenta pretendida |
|---|---|
| snapshots de buffers | `insta` ou equivalente |
| property tests | `proptest` ou equivalente |
| PTY | `portable-pty`; adapter ConPTY nativo somente se um teste demonstrar capability ausente |
| benchmarks | Criterion ou harness do workspace |
| fault injection | adapters fake do `slim-tui-testkit` |

### 5.3 Regra de dependência

Nenhuma dependência entra apenas porque o Grok Build a utiliza. Para cada crate:

1. identificar o boundary que ela resolve;
2. confirmar suporte Windows;
3. verificar impacto no binário e startup;
4. adicionar teste do comportamento esperado;
5. registrar licença.

### 5.4 Estratégia de componentes estilo termcn

Não existe equivalente direto do `termcn` para Rust/Ratatui no conjunto
avaliado. O `termcn` publica componentes React para Ink/OpenTUI; não oferece
crate Rust, widgets Ratatui ou adapter oficial. Portanto:

- não consumir registry `termcn` no build do Slim;
- não adicionar Bun, Node, React, OpenTUI ou Zig ao runtime/release;
- não usar sidecar JS nem segundo proprietário de stdin/raw mode/alternate screen;
- não migrar para `iocraft`, `tui-realm` ou outro framework apenas para obter
  componentes visuais;
- portar comportamento e linguagem visual para código Rust source-owned;
- preservar notices/licença MIT quando houver cópia substancial e registrar
  commit, arquivo estudado e proveniência.

#### Stack recomendada

| Necessidade | Decisão |
|---|---|
| layout, surfaces, borders, gauges, scrollbar | widgets/primitives nativos do Ratatui |
| spinner, streaming caret, progress conhecido | `FrameClock` + widget interno; sem crate de spinner |
| fades, dissolve, drawer e highlight temporário | considerar `tachyonfx` após benchmark |
| Markdown | `pulldown-cmark` com sanitização própria |
| syntax highlight | `syntect` lazy e bounded |
| imagens | `ratatui-image` somente no milestone capability-gated |
| tree | avaliar `tui-tree-widget`; rejeitar se duplicar focus/selection state |
| composer | implementação Slim; não adotar textarea genérico que perca paste atômico |

`tachyonfx` não é requisito para spinner, caret ou progress rail. Esses efeitos
são pequenos e determinísticos com `FrameClock`. A dependência só entra se
transições compostas demonstrarem ganho visual e mantiverem os budgets.

#### Kit interno source-owned

O Slim mantém um kit pequeno, equivalente à filosofia shadcn/termcn: código
copiado/portado pertence ao repositório e segue o theme/reducer local.

```text
Surface
Rail
ModalFrame
ComposerFrame
StateGlyph
ProgressTrack
StreamingText
ThinkingBlock
ToolBlock
CommandPalette
Scrollbar
```

Contrato de cada componente:

```text
ViewModel + Rect + Theme + FrameClock → Buffer
```

Componente não pode executar I/O, iniciar timer, consumir channel, consultar
relógio global, alterar `AppState` ou manter autoridade paralela. Interação gera
`Action`; estado volta pelo reducer.

#### Ordem de port

1. `ThinkingBlock` + `ProgressTrack`/spinner;
2. `StreamingText` + caret reduced-motion-safe;
3. `CommandPalette`;
4. lifecycle visual de tools;
5. componentes adicionais somente quando usados por fluxo real.

Primeiro spike usa somente Ratatui. `tachyonfx` entra numa segunda comparação,
contra a versão nativa, para fade de overlay, focus transition, dissolve,
drawer e highlight temporário.

Gate do spike:

- buffers reais em 40×8, 80×24 e 140×40;
- truecolor, ANSI16 e no-color;
- motion normal e reduced motion sem mudança de layout;
- Unicode/CJK/emoji/combining;
- input→frame p95 dentro de 16 ms;
- resize/shutdown/restore Windows sem segundo event loop;
- ganho visual claro contra implementação Ratatui simples.

## 6. Estrutura lógica do crate

O crate inicial permanece único, com módulos internos:

```text
slim-tui
├── api             tipos públicos e UiCommand/UiEvent
├── app             AppState, Action, reducer e ViewModel
├── runtime         event loop, scheduling, channels e EffectRunner
├── surface         trait + fullscreen
├── terminal        RAII guard, capabilities e platform adapters
├── block           modelo de blocos e renderers
├── component       primitives source-owned inspiradas em termcn
├── render          pipeline, buffers, caches e virtualização
├── layout          breakpoints, LayoutPlan e hit testing
├── composer        editor, history, paste e completion
├── todo_dock       projeção residente e expansão in-place do Todo
├── overlay         stack, focus e modal lifecycle
├── inspector       diff, activity, tree e diagnostics
├── input           keymap, mouse e normalization
├── theme           tokens, glyphs e capabilities
├── markdown        parse e highlight
├── image           protocols e fallback
├── telemetry       métricas sem conteúdo
└── testkit         clocks, backends, fixtures e PTY helpers
```

Não criar um crate separado para cada módulo. A extração exige um consumidor
independente ou redução comprovada de compile time/coupling.

## 7. API entre harness e TUI

### 7.1 Identificadores

Usar newtypes, nunca strings intercambiáveis:

```rust
pub struct SessionId(pub Arc<str>);
pub struct MessageId(pub Arc<str>);
pub struct BlockId(pub Arc<str>);
pub struct ToolCallId(pub Arc<str>);
pub struct OverlayId(pub u64);
pub struct EffectId(pub u64);
```

### 7.2 Eventos recebidos

```rust
pub enum UiEvent {
    SessionSnapshot(SessionSnapshot),
    SessionChanged(SessionMeta),
    RunStarted { run_id: RunId },
    RunEnded { run_id: RunId, outcome: RunOutcome },
    UserMessageAdded(UserMessageView),
    AssistantStarted(AssistantStart),
    AssistantDelta(AssistantDelta),
    AssistantEnded(AssistantEnd),
    ThinkingStarted(ThinkingStart),
    ThinkingDelta(ThinkingDelta),
    ThinkingEnded(ThinkingEnd),
    ToolStarted(ToolStart),
    ToolProgress(ToolProgress),
    ToolEnded(ToolEnd),
    CompactionStarted(CompactionView),
    CompactionEnded(CompactionView),
    ModeChanged(ModeView),
    CapabilitySurfaceChanged(CapabilitySurface),
    PlanChanged(PlanView),
    TodoChanged(TodoView),
    GoalChanged(GoalView),
    ActivityChanged(ActivityView),
    ApprovalRequired(PlanApprovalView),
    InputRequired(InputRequestView),
    StatusChanged(StatusSnapshot),
    QueueChanged(PromptQueueSnapshot),
    ContentPageLoaded(ContentPageView),
    Notification(NotificationView),
    FatalError(FatalUiError),
}
```

Regras:

- cada stream possui sequence monotônica;
- `Ended` carrega o snapshot final integral da entidade;
- delta pode ser coalescido;
- snapshot recupera qualquer gap;
- eventos não carregam secrets não redigidos;
- campos desconhecidos em protocolos externos falham soft quando seguro.

### 7.3 Comandos emitidos

```rust
pub enum UiCommand {
    SendPrompt(PromptDraft),
    EnqueuePrompt(PromptDraft),
    Steer(PromptDraft),
    AbortRun { run_id: RunId },
    CancelTool { tool_call_id: ToolCallId },
    RetryLastTurn,
    SelectModel(ModelSelection),
    SetMode(OperatingMode),
    ApprovePlan { plan_version: PlanVersion },
    AnswerInput { request_id: InputRequestId, answer: Arc<str> },
    SpawnSubagent(SubagentSpawnRequest),
    CancelSubagent { child_id: ChildId },
    ExecuteSlashCommand(SlashInvocation),
    RequestSessionSnapshot,
    NavigateSessionTree { entry_id: EntryId },
    SetTodoState(TodoMutation),
    ChangeSetting(SettingMutation),
    RequestContentPage { handle: ContentHandle, cursor: Option<PageCursor> },
    Shutdown,
}
```

O harness pode rejeitar qualquer comando. Rejeição volta como evento; a TUI não
simula sucesso local.

### 7.4 Contrato de interação da v1

- `CapabilitySurface` carrega o modo atual e o conjunto visível de capabilities:
  `Auto` pode expor tools irrestritas; `Plan`/`Read-only` expõem read/list/search,
  resources e prompts, mas não shell, writes, scripts ou MCP tools.
- `ActivityView` identifica `tool`, `mcp` ou `child`, com id, estado, duração,
  preview, cancelabilidade e expansão.
- `PlanView` carrega versão, passos, dependências, status e `approval_required`.
- `GoalView` carrega estado, budget restante, causa de bloqueio e assurance.
- `InputRequestView` carrega id, pergunta, opções e estado de persistência.

- A sessão começa em `Auto`.
- `Shift+Tab` percorre `Auto → Read-only → Plan → Auto`; `/mode` é o caminho
  textual equivalente.
- `Plan` e `Read-only` exibem o catálogo de capabilities recebido em
  `CapabilitySurfaceChanged`; a TUI não calcula permissões.
- Aprovar um Plan emite `ApprovePlan`; somente o evento do harness confirma a
  versão e a transição para `Auto`.
- `InputRequired` abre um bloco de pergunta e `AnswerInput` envia a resposta;
  em headless, o mesmo estado resulta em `input_required` persistido.
- `ActivityChanged` cobre tool, MCP e subagente. Não existe Jobs manager na TUI;
  atividade transitória é o único inspector operacional da v1.
- reasoning aparece como `ThinkingBlock` recolhido por padrão; expandir é uma
  ação visual e não altera o contexto enviado ao provider.
- mensagens aguardando execução viram `QueuedUserBlock` visível no fim do
  transcript, em ordem FIFO, com estado `queued` até serem consumidas.

## 8. Estado da aplicação

### 8.1 AppState

```rust
pub struct AppState {
    pub session: SessionView,
    pub scrollback: ScrollbackState,
    pub composer: ComposerState,
    pub todo_dock: TodoDockState,
    pub overlays: OverlayStack,
    pub inspector: InspectorState,
    pub focus: FocusState,
    pub viewport: ViewportState,
    pub surface: SurfaceState,
    pub capabilities: SurfaceCapabilities,
    pub theme: ThemeState,
    pub notifications: NotificationState,
    pub keymap: Keymap,
    pub pending_effects: HashMap<EffectId, PendingEffect>,
    pub revisions: RevisionSet,
}
```

### 8.2 SessionView

Contém somente dados necessários à apresentação:

- session id e display name;
- cwd display-safe;
- modelo/provider/profile/effort;
- Mode e capability surface; não existe Permission state;
- context usage e cost;
- run state;
- prompt queue;
- Todo/Plan/Goal summary, incluindo version, budget e assurance;
- activity summary;
- diagnostics counters.

O transcript visual vive em `ScrollbackState`, não duplicado em `SessionView`.

### 8.3 RevisionSet

Revisions determinam invalidação:

```rust
pub struct RevisionSet {
    pub content: u64,
    pub fold: u64,
    pub theme: u64,
    pub viewport: u64,
    pub focus: u64,
    pub status: u64,
}
```

Não usar timestamps como revision. Uma mutação que não muda visualmente não
incrementa a revision correspondente.

## 9. Actions, reducer e effects

### 9.1 Action

```rust
pub enum Action {
    UiEventReceived(UiEvent),
    Key(KeyEvent),
    Paste(PasteEvent),
    Mouse(MouseEvent),
    Resize { width: u16, height: u16 },
    Tick(FrameClock),
    OpenOverlay(OverlaySpec),
    CloseTopOverlay,
    ToggleInspector(InspectorKind),
    ToggleTodoDock,
    ToggleBlock(BlockId),
    Scroll(ScrollIntent),
    Composer(ComposerAction),
    SubmitComposer(SubmitMode),
    EffectCompleted { id: EffectId, output: EffectOutput },
    EffectFailed { id: EffectId, error: EffectError },
    SurfaceLost(SurfaceError),
    RequestShutdown(ShutdownReason),
}
```

### 9.2 Reducer

Assinatura conceitual:

```rust
fn reduce(state: &mut AppState, action: Action) -> Vec<Effect>;
```

Invariantes:

- reducer nunca bloqueia;
- reducer nunca chama async;
- reducer nunca escreve terminal;
- reducer não ignora erro silenciosamente;
- reducer valida IDs/revisions antes de aplicar delta;
- toda mudança visual incrementa revision apropriada;
- actions duplicadas são idempotentes quando o protocolo permitir.

### 9.3 Effect

```rust
pub enum Effect {
    Send(UiCommand),
    CopyToClipboard { text: Arc<str> },
    OpenUrl { url: Arc<str> },
    LoadImage { image_id: ImageId, source: ImageSource },
    PersistUiSetting(SettingMutation),
    RequestRender(RenderUrgency),
    ExitProcess(ExitIntent),
}
```

`RequestRender` é hint; o scheduler pode coalescer. `Send`, completion e errors
não podem ser descartados.

## 10. Event loop e scheduling

### 10.1 Canais

Usar dois lanes bounded:

- **control lane:** input, abort, resize, lifecycle final, fatal error;
- **data lane:** deltas, progress, notifications e ticks.

O control lane sempre é drenado primeiro, mas há fairness determinística: após
no máximo 32 control events consecutivos, processar um data event disponível
antes de voltar ao control lane. Se não houver data, continuar com control.

Capacidades iniciais sugeridas:

- control: 256;
- data: 1.024;
- effects in-flight: 128.

Valores são configuráveis no testkit e viram métricas. Overflow do control lane
é fatal e visível. Overflow do data lane ativa coalescing/resync.

### 10.2 Coalescing

| Evento | Regra |
|---|---|
| assistant text delta | concatenar por `MessageId` e sequence contígua |
| thinking delta | concatenar por `MessageId` e sequence contígua |
| tool progress | manter update mais recente e acumular texto quando append-only |
| resize | manter apenas dimensões mais recentes |
| tick | no máximo um pendente |
| mouse move/scroll | acumular delta dentro da frame window |
| lifecycle start/end | nunca coalescer |
| errors | nunca coalescer |

Antes de aplicar um `Ended`, flushar deltas pendentes da mesma entidade.

### 10.3 Frame scheduling

- input local pede frame imediato;
- primeiro delta de um stream pede frame imediato;
- deltas seguintes respeitam janela máxima de 16 ms;
- completion/error pede frame imediato;
- spinner, streaming caret, progress rail e tool progress usam `FrameClock`;
- motion ativo roda entre 8 e 12 fps; não tentar 60 fps sem medição;
- idle pode manter pulso ambiental de até 2 fps no foco/status, desligável;
- reduced motion ou motion desativado elimina ticks ambientais;
- sem animation e sem estado dirty, não existe timer periódico;
- backpressure suspende frames não urgentes e reduz primeiro motion ambiental;
- ao drenar stdout, emitir o frame mais recente, não cada frame perdido.

### 10.4 Pseudocódigo

```rust
loop {
    let action = scheduler.next_action().await;
    let effects = reduce(&mut state, action);
    effect_runner.spawn_all(effects);

    if render_scheduler.should_render(&state.revisions) {
        let view = ViewModel::derive(&state);
        let plan = layout_engine.plan(&view, surface.size());
        surface.draw(&view, &plan, &mut render_context)?;
        render_scheduler.commit(state.revisions);
    }

    if state.surface.shutdown_ready() {
        break;
    }
}
```

## 11. Modelo de blocos

### 11.1 Estrutura comum

```rust
pub struct Block {
    pub id: BlockId,
    pub source: BlockSource,
    pub created_seq: u64,
    pub content_generation: u64,
    pub display: BlockDisplayState,
    pub kind: BlockKind,
}
```

### 11.2 Variantes

```rust
pub enum BlockKind {
    User(UserBlock),
    QueuedUser(QueuedUserBlock),
    Assistant(AssistantBlock),
    Thinking(ThinkingBlock),
    ToolCall(ToolCallBlock),
    ToolResult(ToolResultBlock),
    System(SystemBlock),
    Compaction(CompactionBlock),
    Plan(PlanBlock),
    Activity(ActivityBlock),
    Error(ErrorBlock),
    Custom(CustomBlock),
}
```

`QueuedUserBlock` preserva o texto integral, posição FIFO, timestamp e estado
`queued`; ele vira `UserBlock` somente quando o harness emitir o evento de
consumo. `PlanBlock` mostra a versão aguardando aprovação ou o resultado da
execução. `ActivityBlock` é a projeção comum de tool, MCP e child.

### 11.3 Display state

```rust
pub struct BlockDisplayState {
    pub fold: FoldState,
    pub lifecycle: BlockLifecycle,
    pub selected: bool,
    pub search_match: Option<SearchMatchState>,
}

pub enum FoldState { Auto, Collapsed, Expanded }
pub enum BlockLifecycle { Pending, Streaming, Complete, Failed, Cancelled }
```

`Auto` é resolvido por tipo e contexto:

- user: expanded;
- queued user: expanded with muted `queued` state and FIFO position;
- assistant: expanded;
- thinking: collapsed, preview de três linhas;
- tool call: summary de uma linha;
- tool result: collapsed quando grande, expanded quando curto/error relevante;
- system/compaction: collapsed com summary;
- plan: expanded when awaiting approval, collapsed after approval;
- activity: one-line summary, expanded in-place;
- error: expanded.

### 11.4 Regras de tool blocks

Cada tool call mostra:

- glyph de lifecycle;
- tool name;
- argumento resumido seguro;
- duração;
- status;
- progress curto;
- hint de expand/cancel quando aplicável.

Output integral não é duplicado na TUI. `ToolResultBlock` mantém um
`ContentHandle`; o renderer acessa apenas preview materializado. Expandir pode
emitir `UiCommand::RequestContentPage`. O harness responde com
`UiEvent::ContentPageLoaded`; páginas são anexadas somente se handle/cursor
corresponderem ao request pendente. Uma resposta stale é ignorada.

#### 11.4.1 Agregação visual por turno

Calls concluídas com sucesso são agregadas **somente para apresentação**, por
`tool name`, dentro de um único assistant turn:

```text
✓ read ×3                                      Enter detalhes
✓ bash ×2                                      Enter detalhes
```

O modelo de blocos continua preservando cada call, argumento, resultado,
duração e `ContentHandle` individual. A projeção agregada não concatena outputs
e não altera replay, telemetria ou contabilidade.

Regras normativas:

- o escopo do grupo termina no próximo assistant/user turn;
- argumento/target não aparece na row agregada; fica nos detalhes;
- pending e running ocupam row própria enquanto estiverem ativos;
- failed e cancelled sempre ocupam row própria e mostram o comando/target
  acionável; nunca ficam escondidos em uma contagem;
- quando uma call running conclui com sucesso, ela pode ser incorporada ao grupo
  existente sem criar nova row;
- o glyph ocupa a coluna da rail; o texto da tool começa no mesmo eixo das
  mensagens;
- `Enter` expande todas as calls do grupo inline, sem overlay e sem scroll
  aninhado; `Enter` novamente recolhe;
- a expansão é completa e pode crescer verticalmente porque só ocorre por ação
  explícita; nenhum dado é truncado.

### 11.5 Custom blocks

Somente em M5:

```rust
pub struct CustomBlock {
    pub schema_version: u16,
    pub renderer_id: Arc<str>,
    pub payload: serde_json::Value,
    pub fallback_text: Arc<str>,
}
```

Sem renderer registrado, usar `fallback_text`. Custom renderer não recebe
terminal handle nem `AppState` mutável.

## 12. Pipeline de renderização

### 12.1 Estágios

```text
Block content
→ parse Markdown/ANSI control-safe
→ syntax highlight
→ logical lines
→ wrap por largura
→ BlockOutput + metadata
→ Entry chrome/padding/accent
→ viewport clipping
→ overlay composition
→ frame buffer
→ backend diff/flush
```

### 12.2 BlockOutput

```rust
pub struct BlockOutput {
    pub lines: Arc<[Line<'static>]>,
    pub logical_line_map: Arc<[LogicalLineRef]>,
    pub hit_regions: Arc<[HitRegion]>,
    pub copy_joiners: Arc<[CopyJoiner]>,
    pub measured_width: u16,
    pub generation: u64,
}
```

`copy_joiners` preserva a diferença entre quebra lógica e quebra visual. Copiar
texto reconstruirá linhas originais, não incluirá glyph de continuação.

### 12.3 Caches

#### ParseCache

Chave:

```text
(BlockId, content_generation, markdown_mode, highlight_theme)
```

Valor: logical styled lines antes de wrap.

#### WrapCache

Chave:

```text
(ParseKey, available_width, fold_state)
```

Valor: `BlockOutput`.

#### LayoutCache

Chave:

```text
(viewport_width, viewport_height, theme_revision, fold_revision,
 todo_dock_state, inspector_state, overlay_revision)
```

Valor: `LayoutPlan` e heights conhecidos.

### 12.4 Limites de cache

Todos os caches são LRU/bounded:

- ParseCache: por bytes aproximados;
- WrapCache: por número de rows + bytes;
- LayoutCache: poucas entradas recentes;
- image decode cache: por bytes;
- syntax definitions: inicialização lazy única.

Eviction nunca remove conteúdo fonte, apenas derivação reproduzível.

Budgets iniciais:

| Cache | High-water | Low-water |
|---|---:|---:|
| ParseCache | 16 MiB | 12 MiB |
| WrapCache | 32 MiB ou 100.000 rows | 24 MiB ou 75.000 rows |
| LayoutCache | 16 plans | 8 plans |
| image decode cache | 64 MiB | 48 MiB |

Ao cruzar qualquer high-water, evictar LRU até o low-water correspondente. Se
um único item exceder o budget completo, não cacheá-lo.

### 12.5 Matriz de invalidação

| Mudança | Parse | Wrap | Layout |
|---|---|---|---|
| novo delta de texto | invalida bloco | invalida bloco | atualiza height |
| largura | preserva | invalida | invalida |
| theme de syntax | invalida code blocks | invalida dependentes | invalida |
| theme só de chrome | preserva | preserva quando linhas não embutem cor | invalida |
| fold | preserva | invalida bloco | invalida |
| scroll offset | preserva | preserva | recalcula clipping |
| focus | preserva | preserva | repaint de chrome |
| overlay | preserva | preserva | recompõe overlay |

## 13. Virtualização e scrollback

### 13.1 M1

M1 pode somar heights linearmente porque o corpus inicial é pequeno. Mesmo
assim, só renderiza blocos que intersectam a viewport.

### 13.2 M2 final

Introduzir `HeightIndex` com prefix sums:

- key por `BlockId` e wrap width;
- height exata quando medida;
- estimate conservadora quando ainda não medida;
- busca do primeiro bloco visível em `O(log n)`;
- updates de height em `O(log n)`;
- anchor estável por `(BlockId, row_offset)`.

Ao mudar largura:

1. capturar anchor atual;
2. invalidar heights daquela width;
3. medir primeiro a região do anchor;
4. reconstruir prefix sums em budget de trabalho;
5. reposicionar anchor;
6. medir off-screen apenas sob demanda.

Isso evita salto para o início durante resize.

### 13.3 Estados de scroll

```rust
pub enum FollowMode {
    LiveEdge,
    Pinned { anchor: ScrollAnchor, unseen_blocks: u32 },
}
```

Transições:

| Estado | Evento | Próximo estado |
|---|---|---|
| LiveEdge | novo bloco/delta | LiveEdge, mantém final visível |
| LiveEdge | scroll para cima | Pinned(anchor, 0) |
| Pinned | novo bloco | Pinned(anchor, +1) |
| Pinned | End | LiveEdge |
| Pinned | scroll até final | LiveEdge |
| qualquer | session replace | LiveEdge |

## 14. Layout responsivo

### 14.1 Regiões fullscreen

```text
├ Context rail: cwd/session ───────────── context/progress ────┤
│ Scrollback                                      │ Inspector? │
│                                                 │            │
├ Activity rail? ─────────────────────────────────┴────────────┤
├ Todo dock? ──────────────────────────────────────────────────┤
╭ Composer: draft ─────────────────── model/mode/effort ──────╮
╰─────────────────────────────────────────────────────────────╯
└ Operational bar: shortcuts/status ──────────────────────────┘
```

`ContextRail` ocupa uma row no topo em altura normal. Mostra cwd/session à
esquerda e contexto/progresso à direita; uma rail colorida fina indica progresso
quando houver operação mensurável. Em altura crítica, ela é a primeira região
não essencial a desaparecer.

`ActivityRail` ocupa zero ou uma row acima do Todo/composer. Só aparece durante
run, retry, tool longa ou input requerido; mostra spinner, duração e ação de
cancelamento sem duplicar transcript. A faixa inferior permanece para shortcuts,
mode/model e estado compacto.

### 14.2 Breakpoints

| Largura | Inspector | Operational bar |
|---:|---|---|
| `<100` | overlay central/full-height | compactos |
| `100–139` | drawer temporário que reduz scrollback | normais |
| `≥140` | drawer simultâneo opcional | completos |

Os breakpoints acima valem para Diff, Activity, Session Tree e Diagnostics. Eles
**não alteram o Todo**.

### 14.3 Todo dock

Quando existe Todo ativo, o dock ocupa toda a largura imediatamente acima do
composer. Não é sidebar, drawer nem overlay.

| Condição | Altura do dock | Conteúdo |
|---|---:|---|
| sem Todo ativo | 0 rows | dock ausente |
| Todo ativo, estado compacto | 2 rows | progresso + item ativo; resumo dos demais |
| terminal com altura `<10` | 1 row | progresso + item ativo truncado |
| estado expandido | até 6 rows | lista completa com scroll interno se necessário |

Regras:

- mesma posição em qualquer largura;
- expandir faz reflow do scrollback; nunca cobre conteúdo;
- items não fazem wrap no modo compacto; usam ellipsis;
- `Ctrl+T` alterna compacto/expandido;
- quando expanded, o dock pode receber foco e setas; `Esc` volta ao compacto;
- o dock é projeção de `TodoState`, nunca autoridade paralela;
- mudança de Todo chega como `UiEvent::TodoChanged`, incrementa
  `todo_dock_revision` e persiste no event log do harness; não exige um novo
  bloco de chat a cada atualização.

### 14.4 Altura

Prioridade quando a altura diminui:

1. preservar ao menos uma linha funcional de composer;
2. preservar operational bar mínimo;
3. reduzir composer boxed de três para uma row compacta;
4. ocultar ContextRail e incorporar contexto crítico na operational bar;
5. ocultar ActivityRail e mover estado ativo para a operational bar;
6. reduzir Todo dock de duas para uma linha;
7. entregar todo espaço restante ao scrollback;
8. overlays aplicam scroll interno.

Nunca renderizar área negativa ou usar `saturating_sub` para esconder erro de
layout. O planner valida invariantes e retorna layout de emergência explícito.

### 14.5 Layout de emergência

Para terminal menor que 40×8:

- esconder inspector, ContextRail e ActivityRail;
- manter Todo ativo em uma linha truncada;
- uma linha de operational bar mínima;
- uma linha de composer sem box;
- scrollback no restante;
- remover ambient motion;
- toast: “terminal muito pequeno”; sem panic.

## 15. Componentes

### 15.1 ContextRail e OperationalBar

`ContextRail` usa uma row no topo:

```text
~/workspace/refactor-auth                          ctx 42% · 9.4k/128k
```

Quando existe progresso mensurável, a própria row desenha `progress_track` e
`progress_fill` sem adicionar painel alto. Cwd/session ficam à esquerda;
contexto, fila ou budget ficam à direita. Não repetir `SLIM` nessa região.

`OperationalBar` usa uma row imediatamente abaixo do composer:

```text
Shift+Tab mode  Ctrl+C cancel  Ctrl+P commands     AUTO · GPT-5.6 Sol · high
```

Shortcuts acionáveis ficam à esquerda. Mode, model, effort e estado crítico
ficam à direita. Durante largura reduzida, remover na ordem: shortcuts não
acionáveis, effort, model e hints; Mode e error/retry permanecem.

Em largura crítica:

```text
Ctrl+C cancel                                  AUTO · ◌
```

Nenhuma row faz wrap. `SLIM` aparece no empty state ou no primeiro assistant
label, não simultaneamente em todas as rails. Ambient idle pode pulsar somente
um glyph/status em até 2 fps; texto e alinhamento permanecem estáveis.

### 15.2 ScrollbackView

Responsabilidades:

- localizar faixa visível;
- pedir outputs ao cache;
- compor block chrome;
- produzir hit regions;
- mostrar live-edge/unseen indicator;
- fornecer seleção/copy/search.

Não interpreta `AgentEvent`; recebe blocks já materializados.

#### 15.2.1 Grade, largura e espaçamento

- user e assistant usam stack compacto: label em row própria e conteúdo logo
  abaixo, sem card completo por mensagem;
- prompt do usuário usa `user_prompt_bg` em toda largura útil, com padding
  horizontal de duas células; não usa borda nem cantos;
- assistant permanece sobre `surface`, com rail ou label em `assistant_accent`;
- nome e mensagem começam no mesmo eixo após rail/padding;
- não existe row vazia obrigatória entre blocos conversacionais consecutivos;
- mensagens usam 100% da largura útil do scrollback: sem `max-width`, coluna
  central ou balões estreitos;
- headings Markdown usam `heading_accent`; links usam `link_accent`; prosa comum
  permanece em `text`;
- a rail define somente o início; texto longo quebra na borda direita;
- linhas de continuação alinham com o texto, nunca com a rail;
- inspector aberto e scrollbar reduzem a largura útil; não mudam a regra;
- timestamp aparece dim e alinhado à direita quando largura `≥100`; some abaixo
  disso. O instante continua metadata para replay/diagnóstico;
- duração de reasoning/tool pode aparecer junto do lifecycle; tokens/custo ficam
  agregados nas rails operacionais;
- scrollbar usa track quase invisível e thumb claro; aparece ao scroll, pinned ou
  hover/foco e reduz opacidade em live edge;
- code e diff preservam linhas e usam viewport horizontal.

### 15.3 Composer

Estado:

- rope/string buffer;
- cursor por grapheme;
- selection;
- preferred column;
- undo/redo bounded;
- history de prompts;
- completion state;
- paste state;
- attachments;
- draft revision.

Limites iniciais:

- draft/paste textual: 1 MiB; acima disso, recusar com mensagem e oferecer
  importação como attachment pelo harness;
- undo/redo: menor entre 200 operações e 4 MiB de deltas;
- prompt history: 100 entradas, sem attachments binários;
- completion candidates: 500 antes de filtro/viewport;
- attachments: apenas metadata/handles na TUI.

Regras:

- altura normal de três rows: box arredondado completo (`BorderType::Rounded`)
  com o label embutido no extremo direito da borda inferior (revisão W6,
  2026-08-21, seguindo a referência visual do Grok Build — decisão do usuário);
- altura compacta de uma row quando viewport não comporta o box;
- `composer_bg` diferencia input do transcript sem usar sombra ou gradiente;
- borda neutra (`border`) em todos os estados; foco sinalizado apenas pelo
  glifo `›` em `accent` (muted quando ocioso) — nenhuma borda muda de cor;
- model/mode/effort ocupam o label da borda inferior à direita, em `muted`;
- texto digitado usa viewport horizontal em torno do cursor; não faz wrap;
- draft multiline continua permitido; a row mostra a linha lógica do cursor e
  um indicador `linha/total` quando houver mais de uma;
- Enter envia quando completion/modal não captura;
- Shift+Enter insere newline quando distinguível;
- Ctrl+Enter também insere newline como fallback Windows configurável;
- Alt+Enter envia como steer quando run ativo;
- paste multiline nunca envia automaticamente;
- draft é preservado se send falhar;
- secret-looking paste não entra em telemetry.

Bracketed paste é armazenado como `PasteSegment`, separado do texto digitado.
O renderer mostra o conteúdo literal apenas quando ele cabe em uma row e não
contém newline. Caso contrário, mostra um segmento atômico:

```text
[Pasted Content 1925 chars]
```

`chars` conta Unicode scalar values, nunca bytes. O texto real permanece no
draft e é enviado integralmente. Cursor pula o segmento como uma unidade;
Backspace/Delete sobre ele remove o paste inteiro; `Ctrl+Z` restaura/remove a
operação inteira. Resize pode alternar entre literal e token sem alterar o
estado armazenado.

Quando o composer tem foco, `›`, cursor e borda ativa usam
`assistant_accent`/`border_focus`. O background muda de `surface_alt` para
`composer_bg`; essa mudança é semântica e estável, não um flash. Streaming ativo
pode mostrar caret animado no assistant block, nunca dentro do draft.

### 15.4 Sinais operacionais e ActivityRail

`ActivityRail` materializa sinais transitórios: working/retry, tool em execução,
awaiting input, elapsed, fila e ação de cancelamento. Possui uma row e desaparece
quando deixa de ser acionável:

```text
◒ Responding · 15s                              9.45k · Ctrl+C stop
```

Spinner usa sequência estável de glyphs e 8–12 fps em motion normal. Tool com
progresso conhecido usa `progress_track/progress_fill`; sem progresso conhecido,
usa spinner e elapsed, nunca barra falsa.

`OperationalBar` mantém shortcuts, Mode, model/effort, Goal curto e unseen count.
Quando há Goal ativo, mostra `active`, `paused` ou `blocked`, budget restante e
`verified`/`unverified`. Goal completo expande in-place no transcript, não vira
painel permanente.

### 15.5 TodoDock

`TodoDockState`:

```rust
pub struct TodoDockState {
    pub todo: Option<TodoView>,
    pub expanded: bool,
    pub selected_index: usize,
    pub scroll_offset: usize,
    pub revision: u64,
}
```

Render compacto:

```text
TODO 2/4  ◌ Rodar testes afetados
✓ Mapear fluxo · ✓ Corrigir deduplicação · ○ Revisar diff
```

O renderer corta cada row na largura disponível; não empurra o composer para
fora e não altera a largura do chat.

O Todo não possui cor de identidade própria. Contador é neutro; `✓` concluído
usa success, `◌` ativo usa warning, `○` pendente usa muted e `✕` falho usa error.

### 15.6 Inspector

`InspectorKind`:

```rust
pub enum InspectorKind { Diff, Activity, SessionTree, Diagnostics }
```

Cada inspector tem ViewModel próprio e comandos tipados. Nenhum lê estado
global por singleton.

### 15.7 OverlayStack

```rust
pub struct OverlayEntry {
    pub id: OverlayId,
    pub kind: OverlayKind,
    pub capture: CapturePolicy,
    pub anchor: OverlayAnchor,
    pub size: OverlaySize,
    pub restore_focus: FocusTarget,
}
```

Stack visual e focus order são iguais. Overlay não-capturante não recebe tecla,
mas pode receber hit test quando explicitamente permitido.

Máximo de 16 overlays simultâneos. Ao atingir o limite, rejeitar o novo overlay,
registrar diagnóstico e manter o stack existente.

### 15.8 Toasts

- máximo três visíveis;
- fila bounded em 100; ao exceder, descartar primeiro info expirável, nunca
  error não reconhecido;
- error não expira automaticamente enquanto não reconhecido;
- info/success expiram usando clock injetado;
- reduced motion desativa transições, não duração.

## 16. State machines

### 16.1 Assistant streaming

```text
Absent
→ Streaming
→ Complete
  ├→ Failed
  └→ Cancelled
```

Regras:

- delta antes de start cria placeholder diagnosticado e pede snapshot;
- sequence repetida é ignorada;
- gap de sequence pede snapshot;
- end sempre substitui conteúdo parcial pelo snapshot final;
- end flusha coalescer.

### 16.2 Tool lifecycle

```text
Pending → Running → Complete
                  ├→ Failed
                  └→ CancelRequested → Cancelled|Complete|Failed
```

Cancel é request, não fato consumado. A UI só mostra `Cancelled` ao receber
evento final.

### 16.3 Focus

Prioridade:

```text
capturing overlay > inspector/drawer > Todo dock focado > composer > scrollback
```

Ao fechar overlay, restaurar `restore_focus` se ainda existir; caso contrário,
composer.

### 16.4 Surface lifecycle

```text
Stopped → Entering → Active → Leaving → Stopped
                    └→ Failed → Restoring → Stopped
```

Na v1 existe somente `FullscreenBackend`; não há troca de surface nem comando
`/surface`. Uma futura surface não pode ser adicionada sem nova decisão de
produto e novo contrato de lifecycle.

## 17. Input e keybindings

### 17.1 Normalização

Input bruto é normalizado em:

```rust
pub enum InputEvent {
    Key(NormalizedKey),
    Paste(Arc<str>),
    Mouse(NormalizedMouse),
    FocusGained,
    FocusLost,
    Resize(Size),
}
```

Kitty/CSI-u é capability, não requisito. O mesmo `KeyAction` pode ter aliases
por terminal.

### 17.2 Keymap default

| Ação | Default |
|---|---|
| enviar | Enter |
| newline | Shift+Enter; Ctrl+Enter fallback |
| steer durante run | Alt+Enter |
| fechar/cancelar nível atual | Esc |
| abortar run | Ctrl+C quando run ativo e nenhum modal captura |
| sair | Ctrl+C quando idle e composer vazio; confirmação se necessário |
| help | F1 ou `?` via command palette |
| command palette | Ctrl+P |
| trocar Mode | Shift+Tab; `/mode` |
| model | Ctrl+L |
| expandir/recolher Todo dock | Ctrl+T |
| diff inspector | Ctrl+D |
| activity inspector | Ctrl+J |
| session tree | Ctrl+R |
| search scrollback | Ctrl+F |
| live edge | End |
| expand/collapse block | Enter sobre bloco focado |

Atalhos são configuráveis por ação sem permitir conflito silencioso. Config
duplicada gera diagnóstico e mantém o primeiro binding válido.

### 17.3 Esc cascade

1. fechar autocomplete;
2. fechar modal capturante;
3. fechar drawer/inspector;
4. recolher Todo dock expandido;
5. cancelar seleção/search;
6. pedir cancelamento de tool focada;
7. pedir abort do run;
8. não sair silenciosamente.

## 18. Mouse e hit testing

Mouse é opcional e desligável.

Suportar:

- wheel/trackpad scroll;
- click em block/tool/Todo dock/inspector/tab;
- drag de scrollbar;
- seleção interna somente se não conflitar com seleção nativa;
- Shift como escape para seleção do emulador quando aplicável.

`LayoutPlan` produz `HitRegion { rect, target, z_index }`. Hit test percorre por
z-index. Nunca reconstruir layout dentro do handler de mouse.

Scroll events são acumulados por frame e limitados a metade da viewport para
evitar saltos extremos.

## 19. Markdown, ANSI e Unicode

### 19.1 Sanitização

Texto de modelo/tool é não confiável. Antes do parse:

- remover terminal control sequences não permitidas;
- permitir somente SGR/links quando o source é explicitamente ANSI-safe;
- impedir OSC clipboard/title vindos de conteúdo;
- limitar tamanho por bloco/materialização;
- preservar texto original para copy em store seguro.

### 19.2 Unicode

- cursor move por grapheme, não byte/char;
- wrap usa display width;
- nunca dividir wide char no limite;
- combining mark acompanha base;
- tabs expandem com tab stop configurado;
- invalid UTF-8 recebido de processo vira replacement controlado no adapter,
  antes da TUI.

### 19.3 Highlight

- carregar syntax set sob demanda;
- cachear por language/content generation/theme;
- desconhecido usa plain code style;
- erro de highlighter volta plain, não quebra bloco;
- blocos colapsados não fazem highlight/wrap integral antes de expandir.

### 19.4 Code blocks e diffs

Code block comum usa `code_bg`, ligeiramente distinto do transcript. Uma rail
neutra e label dim separam o bloco da prosa:

```text
│ rust · src/auth/session.rs
│ let token = refresh.lock().await;
│ refresh_once(token).await?;
```

Não usar cantos decorativos nem box completo por padrão. `code_bg` e a rail
fornecem profundidade suficiente. A label contém somente linguagem e path curto.
Overflow horizontal usa viewport; nunca reflow de código.

Diff real é a única exceção com background. O renderer usa:

- background verde/vermelho quase preto em toda row adicionada/removida;
- background mais intenso somente no trecho intraline alterado;
- `+`/`-` e line number sempre presentes, para não depender só de cor;
- duas linhas de contexto antes e depois de cada hunk por padrão;
- gaps como `⋯ N linhas sem alteração`;
- `Enter` expande/recolhe hunk ou arquivo inline;
- o diff integral permanece disponível no Diff inspector.

Código inline sugerido pelo modelo que não corresponde a um diff conhecido não
recebe background de add/remove.

### 19.5 Prosa e headings

Prosa Markdown usa `text`; ênfase e listas variam peso/estilo. Headings usam
`heading_accent` azul frio, links usam `link_accent`, e o título/label principal
do assistant usa `assistant_accent` verde. Não colorir parágrafos inteiros nem
introduzir novas cores por nível de heading.

## 20. Imagens

M3 adiciona suporte capability-gated:

```rust
pub enum ImageProtocol { Kitty, ITerm2, Sixel, Placeholder }
```

Regras:

- detectar capability uma vez por surface enter;
- resize antes de encode;
- limitar bytes/dimensões;
- cachear imagem processada por hash + tamanho em células;
- deletar imagem terminal quando frame que a contém deixa de existir;
- inline backend pode usar placeholder mesmo quando fullscreen suporta imagem;
- alt text sempre existe;
- falha de protocolo mostra placeholder e diagnóstico.

Entrada de imagem na TUI:

- path digitado, clipboard e drag-and-drop que o terminal normalize como path
  produzem um attachment chip no composer;
- o runtime copia/deduplica a imagem por hash na sessão; a TUI guarda apenas
  `ImageId` e preview;
- modelo sem visão produz erro explícito no transcript, sem descartar o anexo;
- o chip pode ser removido antes do envio e sua remoção é uma ação reversível do
  composer.

Windows M1 não depende de imagem inline.

## 21. Themes e glyphs

### 21.1 Tokens semânticos

```rust
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub surface_elevated: Color,
    pub composer_bg: Color,
    pub user_prompt_bg: Color,
    pub text: Color,
    pub muted: Color,
    pub secondary_text: Color,
    pub accent: Color,
    pub heading_accent: Color,
    pub link_accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub border: Color,
    pub border_focus: Color,
    pub operational_divider: Color,
    pub progress_track: Color,
    pub progress_fill: Color,
    pub scrollbar_track: Color,
    pub scrollbar_thumb: Color,
    pub code_rail: Color,
    pub selection: Color,
    pub user_accent: Color,
    pub assistant_accent: Color,
    pub thinking_accent: Color,
    pub tool_accent: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_add_bg: Color,
    pub diff_remove_bg: Color,
    pub diff_add_emphasis_bg: Color,
    pub diff_remove_emphasis_bg: Color,
    pub code_bg: Color,
}
```

Componentes não referenciam cores literais.

### 21.2 Capabilities

- truecolor;
- 256 colors;
- 16 colors;
- no-color;
- Unicode glyphs;
- ASCII fallback;
- reduced motion.

Theme resolve para capabilities na entrada da surface. A cache guarda o theme
resolvido, não `auto`. Em 16 colors/no-color, níveis de surface podem colapsar;
bordas, labels, padding e glyphs preservam hierarquia sem depender do RGB.

### 21.3 Theme default aprovado

Paleta truecolor normativa:

| Token | Valor | Uso |
|---|---|---|
| `background` | `#080A0D` | fundo mais profundo do terminal |
| `surface` | `#0D1014` | transcript e região principal |
| `surface_alt` | `#12161B` | rails, docks e regiões secundárias |
| `surface_elevated` | `#191E24` | overlays, drawers e prompt destacado |
| `composer_bg` | `#151A20` | composer focado/ativo |
| `user_prompt_bg` | `#1B2026` | faixa integral da mensagem do usuário |
| `text` | `#C6CDD5` | texto principal (cinza-suave; branco puro sofre halation e parece negrito) |
| `muted` | `#747B84` | metadata, pendente e hints |
| `secondary_text` | `#A9B0B8` | labels, counters e paste token |
| `accent` | `#78D99B` | identidade principal Slim |
| `heading_accent` | `#82AFFF` | headings e estrutura de resposta |
| `link_accent` | `#8CB4FF` | links e targets navegáveis |
| `border` | `#2B3139` | separadores e boxes funcionais |
| `border_focus` | `#4F7D5E` | composer/drawer focado |
| `operational_divider` | `#252B33` | rails operacionais |
| `progress_track` | `#262D35` | trilho de progresso |
| `progress_fill` | `#78D99B` | progresso conhecido e saudável |
| `scrollbar_track` | `#151A20` | trilho quase invisível da scrollbar |
| `scrollbar_thumb` | `#4B5563` | posição da viewport |
| `user_accent` | `#A9B0B8` | label/rail do usuário |
| `assistant_accent` | `#78D99B` | label Slim, cursor e foco |
| `thinking_accent` | `#9AA4AF` | reasoning e elapsed |
| `tool_accent` | `#7DCFFF` | tools e activity estrutural |
| `success` | `#78D99B` | conclusão/healthy |
| `warning` | `#E6B450` | running/ativo/atenção |
| `error` | `#F07178` | falha/cancelamento relevante |
| `code_rail` | `#46505C` | rail de code/output expandido |
| `selection` | `#263A30` | seleção/foco textual |
| `diff_add` | `#78D99B` | `+` e metadata de adição |
| `diff_remove` | `#F07178` | `-` e metadata de remoção |
| `diff_add_bg` | `#0B1A10` | row adicionada |
| `diff_remove_bg` | `#1C0D11` | row removida |
| `diff_add_emphasis_bg` | `#163D22` | trecho intraline adicionado |
| `diff_remove_emphasis_bg` | `#421820` | trecho intraline removido |
| `code_bg` | `#0B0E12` | code comum separado do transcript |

Componentes não podem inferir semântica a partir do RGB resolvido; usam tokens.

### 21.4 Bordas, densidade e glyphs

- sem borda externa da aplicação;
- boxes são funcionais: composer, overlays e drawers; não envolver cada mensagem;
- profundidade vem de surfaces near-black e borders, não de sombras simuladas;
- user prompt usa faixa elevada sem cantos; assistant usa surface principal;
- ContextRail, ActivityRail e OperationalBar têm uma row cada;
- composer tem três rows normais e uma row no modo compacto/emergência;
- rails verticais aparecem em assistant/tool/code quando ajudam lifecycle;
- zero rows vazias obrigatórias entre user, assistant e tool rows;
- tool rows têm uma row no estado colapsado;
- números dinâmicos usam largura/tabulação estável para não deslocar layout;
- glyphs normativos: `✓` complete, `◒`/`◌` running, `○` pending, `✕` failed,
  `›` composer;
- fallback ASCII: `+`, `~`, `o`, `x`, `>` respectivamente;
- no-color mantém significado por glyph, label e texto de status;
- reduced motion congela spinner, remove pulso/caret e mantém status textual;
- motion nunca altera largura, alinhamento ou posição de texto.

O default é near-black estratificado com profundidade contida. Themes
alternativos podem mudar tokens, nunca remover hierarquia entre surfaces,
agregação de tools ou prioridades responsivas.

## 22. FullscreenBackend

### 22.1 Entrada

Ordem normativa:

1. capturar estado anterior;
2. ativar UTF-8/VT no Windows;
3. ativar raw mode;
4. entrar alternate screen;
5. ativar bracketed paste;
6. ativar focus events/mouse se configurados;
7. esconder cursor até o primeiro frame focável;
8. instalar guard de restore;
9. executar primeiro draw.

Se um passo falha, reverter os anteriores na ordem inversa.

### 22.2 Draw

- obter size atual;
- construir `LayoutPlan`;
- renderizar para buffer Ratatui;
- posicionar cursor do composer;
- `Terminal::flush()` envia diff;
- commit de revisions somente após write bem-sucedido.

### 22.3 Saída

Ordem inversa, idempotente:

1. mostrar cursor;
2. desativar mouse/focus/paste;
3. sair alternate screen;
4. desativar raw mode;
5. restaurar console flags;
6. flush final.

Drop pode tentar restore, mas shutdown explícito é o caminho principal.

## 23. InlineBackend (histórico; não planejado)

Este capítulo descreve uma possibilidade histórica do design. M4/inline não
pertence ao produto atual, não entra em M0–M3 e não pode ser usado como base para
criar um backend fullscreen.

Modelo:

- blocos completos e imutáveis podem ser committed ao scrollback nativo;
- região viva contém streaming, composer, operational bar e overlays
  compatíveis; não reintroduz header no topo;
- backend mantém anchor e número de linhas da região viva;
- renderiza offscreen para Buffer;
- converte região para ANSI;
- escreve somente diff da região viva;
- resize invalida região e faz redraw controlado;
- troca de backend nunca reaproveita cursor bookkeeping do outro backend.

Overlays que exigem screen coordinates podem degradar para selector inline.
Cada overlay declara fallback obrigatório.

## 24. Falhas e recuperação

### 24.1 Render fault por bloco

Se um renderer de bloco falha:

- capturar tipo do bloco e error class;
- registrar métrica sem conteúdo sensível;
- renderizar bloco fallback limitado;
- contar falhas consecutivas;
- desabilitar custom renderer após limite;
- manter restante do frame.

### 24.2 Falha global de frame

- não commit revisions;
- preservar último frame válido;
- invalidar caches envolvidos;
- tentar um full redraw sem feature opcional;
- se repetir, restaurar terminal e encerrar com stderr claro.

### 24.3 Falha de effect

- manter draft/estado autoritativo;
- publicar `EffectFailed`;
- mostrar error block/toast conforme severidade;
- oferecer retry somente quando operação for idempotente.

### 24.4 Gap de eventos

- detectar sequence gap;
- marcar entidade `desynced`;
- pedir `SessionSnapshot`;
- continuar mostrando último estado conhecido com indicador;
- snapshot substitui estado derivado;
- nunca inventar completion.

### 24.5 Panic/signal

Panic hook não escreve UI complexa. Ele:

1. aciona restore idempotente;
2. escreve resumo em stderr;
3. preserva crash artifact conforme policy do harness.

Signals usam o mesmo shutdown path; não duplicar sequência terminal.

## 25. Windows-only

M1 deve provar no Windows:

- Console UTF-8;
- VT input/output;
- raw mode restore;
- Shift+Tab e modificadores;
- bracketed paste;
- resize durante streaming;
- Ctrl+C/Esc;
- cursor/IME no composer;
- terminal close/crash;
- stdout síncrono sem falso backpressure;
- Windows Terminal e pelo menos um terminal alternativo documentado.

Não assumir que `stdout.write(false)` significa bytes pendentes em console
síncrono. O adapter verifica buffer real antes de entrar em estado de
backpressure.

## 26. Observabilidade

Sem capturar prompt/output por default.

Métricas:

- input-to-frame latency;
- render duration por estágio;
- frames requested/rendered/coalesced/dropped;
- cache hit/miss/eviction por cache;
- visible blocks/rows;
- event lane occupancy/high-water;
- snapshot resync count;
- render faults;
- terminal restore attempts/failures;
- bytes written por frame;
- full redraw reasons;
- RSS opcional no benchmark, fora do hot path.

Trace de frame usa correlation ID, surface kind e revisions, nunca conteúdo.

## 27. Budgets de desempenho

Budgets iniciais, medidos em release numa máquina Windows de referência:

| Métrica | Budget |
|---|---:|
| input→frame p95, cache warm | `≤16 ms` |
| janela máxima de streaming | `16 ms` |
| motion ativo | `8–12 fps`; degrada primeiro sob backpressure |
| primeiro delta | frame imediato |
| resize/full redraw p95, 3k blocos/5 MiB | `≤50 ms` |
| idle com ambient motion | `≤2 redraws/s`; `0` com reduced motion/off |
| control event loss | `0` |
| lifecycle event loss | `0` |
| cache/event queue | bounded em todos os caminhos |
| restore em fault tests | `100%` |

Se um budget não for atingido:

1. salvar benchmark com commit/máquina;
2. identificar estágio pelo trace;
3. corrigir gargalo medido;
4. não aumentar budget sem decisão registrada.

## 28. Testes

### 28.1 Unit

- reducer por Action;
- focus cascade;
- scroll transitions;
- block lifecycle;
- tool cancellation;
- overlay stack;
- breakpoints;
- cache keys/invalidation;
- Unicode width/graphemes;
- keymap conflicts;
- error fallback.

### 28.2 Property

Gerar sequências de Actions e garantir:

- focus sempre aponta para target existente;
- overlay stack e z-order não divergem;
- scroll offset permanece válido;
- End retorna ao live edge;
- terminal size pequeno não causa panic;
- sequence gaps nunca produzem completion;
- reducer não perde draft após effect failure;
- cache eviction não muda output.

### 28.3 Golden buffers

Matriz:

- widths 40, 80, 99, 100, 139, 140, 200;
- heights 8, 12, 24, 40;
- light/dark/no-color;
- Unicode/ASCII;
- collapsed/expanded;
- pending/streaming/complete/failed/cancelled;
- overlay/drawer;
- search selection;
- long lines/CJK/emoji/combining.
- mensagens longas usando toda a largura útil e continuação alinhada ao texto;
- tools agregadas por nome, expansão completa e exception rows de falha;
- composer boxed com uma row editável, draft multiline e paste atômico em várias larguras;
- Todo compacto/expandido junto da operational bar inferior;
- diff com contexto dobrado, row tint, intraline emphasis e no-color fallback;
- cinco níveis de surface distinguíveis sem depender somente de cor;
- user prompt elevado, assistant sobre surface e code em `code_bg`;
- composer boxed normal e composer compacto em baixa altura;
- ContextRail/ActivityRail/OperationalBar em estados wide/compact/hidden;
- snapshots de spinner, streaming caret e progress rail em clocks determinísticos;
- reduced motion sem diferenças de layout ou conteúdo;
- scrollbar em live edge, pinned, hover/focus e no-color.

### 28.4 PTY E2E

M1 Windows:

- enter/leave;
- type/send;
- streaming;
- resize;
- paste;
- paste longo renderizado como segmento atômico, sem auto-submit, com
  Backspace/Delete/Ctrl+Z;
- abort;
- crash restore;
- cursor position.

M4 Unix (referência histórica, não planejado):

- signals/suspend-resume;
- inline scrollback;
- Kitty keyboard;
- tmux/SSH matrix quando disponível.

### 28.5 Fault injection

- renderer panic/error;
- terminal write partial/failure;
- channel full;
- effect timeout;
- snapshot gap;
- malformed ANSI;
- image decode failure;
- clipboard unavailable;
- restore step failure.

## 29. Benchmarks

### 29.1 Corpus long-session

- 3.000–3.200 blocos;
- aproximadamente 5 MiB de conteúdo;
- Markdown, code fences, diffs, tools, thinking e errors;
- mistura de collapsed/expanded;
- Unicode realista.

### 29.2 Cenários

- first paint cold;
- steady-state sem mudança;
- append de delta;
- scroll contínuo;
- jump para live edge;
- expand/collapse;
- open/close drawer;
- overlay composition;
- resize 80↔200;
- theme switch;
- search highlight;
- stdout backpressure;
- cache eviction;
- fullscreen steady-state; inline permanece fora dos gates v1.

### 29.3 Metodologia

- separar setup/parse de steady-state;
- aquecer caches quando o cenário mede hot path;
- medir cold path separadamente;
- registrar allocations e bytes escritos;
- fixar corpus e seed;
- publicar máquina, OS, terminal, profile e commit;
- comparar p50/p95, não só média.

## 30. Milestones normativos

Cada milestone recebe seu próprio plano de implementação, execução e gate. A
TUI v1 cobre M0–M3. M4 não é planejado; M5 só pode tratar hardening interno,
sem criar extensibility/plugin system.

### M0 — contratos e testkit

> **Checkpoint atual — quase completo.** Reducer é a única rota de mutação no
> binário (`Action → reduce → Effect`), com EffectRunner mínimo executando
> `Send`; `RevisionSet` tem os 6 campos normativos; IDs de bloco são
> monotônicos; Tool* chegam tipados; proptest cobre scroll/draft/layout. Faltam
> `SurfaceBackend` trait compartilhada, sequences por stream e as variantes
> Plan/Compaction/Custom do block model.

Entregas:

- tipos públicos;
- AppState/Action/Effect;
- reducer;
- ViewModel;
- Block model;
- SurfaceBackend trait;
- MemorySurface fake;
- clocks e IDs determinísticos;
- unit/property tests.

Não inclui terminal real.

Gate: uma sessão fake inteira materializa frames determinísticos sem I/O.

### M1 — fullscreen Windows

> **Checkpoint atual — fatia ampla; gate físico pendente.** Paleta §21.3 aplicada
> com superfícies pintadas (background/surface/surface_alt/composer_bg/
> user_prompt_bg), ContextRail e ActivityRail de uma row, composer boxed de três
> rows com label model·effort e `border_focus`, scrollback navegável via offset
> pin/live-edge/unseen, layout de emergência, mouse capability-gated, UTF-8/VT
> console flags com restore exato, cursor por grapheme da linha lógica. Golden
> matrix roda sobre TestBackend em todas as larguras/alturas normativas.
> Pendente: PTY/ConPTY E2E físico (teste escrito e `#[ignore]`) e validação
> manual Windows Terminal + alternativo da matriz §25.

Entregas:

- TerminalGuard;
- FullscreenBackend;
- ContextRail, scrollback, ActivityRail, composer boxed e operational bar;
- user/assistant/system/error blocks;
- input Windows;
- resize;
- theme near-black estratificado e motion básico;
- golden + PTY Windows.

Não inclui tools reais, mouse, imagens ou drawer.

Gate: abrir, conversar com provider fake, provar hierarchy visual
ContextRail/scrollback/composer/operational bar em truecolor e fallback, resize
e sair sem corromper terminal.

### M2 — integração com harness e performance

> **Checkpoint atual — integração central + pipeline essencial entreges.** Lanes
> bounded control 256 / data 1024 com fairness 32 drenando control primeiro;
> coalescer de deltas participando do runtime real; tool blocks tipados com
> agregação por nome/turno e exception rows para falha/cancelamento;
> `HeightIndex` prefix sums com locate O(log n), WrapCache bounded e render
> virtualizado (só blocos visíveis são materializados); bench long-session
> desenha o pipeline Ratatui final via TestBackend e impõe o budget §27 como
> exit code (p95 release medido: ~2 ms contra 16 ms). Pendente: Todo dock
> alimentado por eventos reais do harness, ParseCache/LayoutCache dedicados e
> métricas §26 exportadas.

Entregas:

- UiEvent/UiCommand reais;
- streaming/coalescing;
- tool blocks/progress/cancel;
- Todo dock fixo compacto/expandido;
- scroll pin/live edge;
- Parse/Wrap/Layout caches;
- virtualização/HeightIndex;
- métricas locais e long-session bench.

Gate: budgets M2 atingidos ou blocker documentado com trace reproduzível; os
goldens também provam `UiEvent`/`UiCommand` para queue, compaction, tool,
cancelamento e Todo dock.

### M3 — experiência completa fullscreen

> **Checkpoint atual — scaffold inicial; redesign visual aprovado.** Theme
> semântico e overlays de login/modelo estão integrados, mas ainda usam linguagem
> visual plana. Inspector state, command palette e image fallback existem apenas
> como helpers isolados. Aplicação da paleta estratificada, motion system, Diff,
> Activity, SessionTree, Diagnostics, palette ligada, mouse, clipboard, search,
> reduced motion, imagens e fluxos Plan/Goal/Input ainda não passaram por
> state/input/render end-to-end. M3 não deve ser tratado como concluído antes de
> finalizar fundação visual e M2.

Entregas:

- inspectors adaptativos diff/activity/tree/diagnostics;
- overlays/modals;
- command palette;
- themes completos com surfaces estratificadas;
- motion system, progress rail, spinner e streaming caret;
- mouse;
- reduced motion/glyph fallback;
- clipboard/search;
- imagens capability-gated.

Gate: matriz golden completa e fault injection sem terminal leak, incluindo:

- `Auto → Read-only → Plan → Auto` por `Shift+Tab`;
- Plan awaiting approval e transição confirmada para Auto;
- Goal active/paused/blocked/verified/unverified;
- queued user blocks em FIFO de oito itens;
- atividade de tools, MCP e subagentes em bloco expansível;
- reasoning recolhido, clipboard/path image attachment e erro de modelo sem
  visão.

### M4 — não planejado

Inline, Unix PTY, tmux e parity entre surfaces não fazem parte do produto atual.

### M5 — hardening interno opcional

Entregas:

- CustomBlock schema somente se exigido pelo próprio core;
- renderer registry interno;
- widgets limitados;
- API stability policy;
- benchmark regression gate;
- documentação interna de manutenção; não criar marketplace, ABI ou plugin host.

Gate: custom renderer não pode quebrar loop, terminal lifecycle ou outros
blocos.

## 31. Regras para o agente implementador

Esta seção é normativa e deliberadamente explícita.

1. Leia o milestone inteiro antes de editar.
2. Não implemente item de milestone posterior.
3. Não adicione crate sem justificar boundary e custo.
4. Não coloque I/O no reducer ou render.
5. Não crie singleton de AppState, terminal ou theme.
6. Não marque cancelado antes do evento final do harness.
7. Não descarte lifecycle event.
8. Não use `sleep` em testes quando um evento determinístico puder sinalizar.
9. Escreva primeiro o teste do contrato observável.
10. Faça o menor slice vertical que produz frame verificável.
11. Rode unit/golden do módulo tocado antes de PTY/full gate.
12. Preserve Windows como gate permanente do produto atual.
13. Se a biblioteca não suportar um requisito, escreva adapter; não vaze a
    limitação para AppState.
14. Se a especificação parecer contraditória, pare e reporte os dois trechos;
    não escolha silenciosamente.
15. Não declare performance sem benchmark do cenário correspondente.
16. Não altere budgets para fazer teste passar.
17. Não copie código do Grok Build sem auditoria de licença/notices.

## 32. Checklist de revisão por PR

- [ ] mudança pertence ao milestone atual;
- [ ] boundary público é menor que a implementação;
- [ ] reducer continua síncrono/puro de I/O;
- [ ] render continua puro;
- [ ] channels/caches continuam bounded;
- [ ] erros são visíveis e não inventam estado;
- [ ] lifecycle terminal permanece idempotente;
- [ ] Windows foi exercitado quando aplicável;
- [ ] golden novo cobre state visual, surface hierarchy e motion clock tocados;
- [ ] PTY cobre interação/lifecycle novo;
- [ ] benchmark cobre hot path alterado;
- [ ] telemetry não captura conteúdo sensível;
- [ ] docs e keymap foram atualizados;
- [ ] nenhuma dependência entrou sem licença registrada.

## 33. Definição de pronto final

> **Checkpoint atual: parcialmente atendida.** Fluxo normativo, fundação visual
> estratificada, scroll virtualizado, tool blocks tipados, lanes bounded,
> markdown-light, palette, motion básico, proptest/fault/golden e benchmark com
> gate estão entregues e verdes. Permanecem pendentes: gate físico Windows
> (PTY/console real), Todo/Plan/Goal end-to-end pelo harness, inspectors/search/
> clipboard/imagem, métricas exportadas e resync por snapshot.

O design da TUI v1 está implementado quando:

- fullscreen Windows restaura terminal em todos os exits testados;
- streaming, tools e scroll não violam budgets;
- hierarquia visual entre background/surfaces/composer/overlays permanece
  legível em truecolor, 256, 16 e no-color;
- ContextRail, ActivityRail e composer boxed degradam sem perder informação;
- Todo dock permanece acima do composer em todos os tamanhos testados;
- drawers/overlays dos demais inspectors funcionam em todos os breakpoints;
- Markdown/Unicode/ANSI não quebram width/cursor;
- event gaps recuperam por snapshot;
- caches são bounded e semanticamente transparentes;
- custom blocks falham isolados;
- long-session benchmarks têm gate versionado;
- nenhuma feature de domínio tornou a TUI sua autoridade;
- documentação pública explica shortcuts, surface fullscreen, hierarchy visual,
  motion/reduced motion, themes e capabilities opcionais.

## 34. Referências de inspiração

- [Grok Build: dependências de rendering](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/Cargo.toml#L845-L883)
- [Grok Build: pipeline e caches de render](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/benches/bench.md#L209-L223)
- [Grok Build: crate principal da TUI](https://github.com/xai-org/grok-build/blob/d92c5b0b8582fda358de1f97446aa74af44a464f/crates/codegen/xai-grok-pager/src/lib.rs)
- [Ratatui](https://github.com/ratatui/ratatui)
- [Crossterm](https://github.com/crossterm-rs/crossterm)
- [termcn](https://github.com/shadcn-labs/termcn) — referência principal de componentes; port seletivo, sem dependência direta
- [OpenTUI](https://github.com/anomalyco/opentui) — referência de primitives/keymaps; runtime não adotado
- [Tokscale](https://github.com/junhoyeo/tokscale) — referência operacional baseada em Ratatui
- [Critique](https://github.com/remorses/critique) — referência para diff responsivo e fuzzy picker
- [tachyonfx](https://github.com/junkdog/tachyonfx) — candidato opcional para transições compostas
- [ratatui-image](https://github.com/benjajaja/ratatui-image) — candidato M3 para imagens capability-gated

Estas referências orientam mecanismos e fronteiras. A implementação do Slim
deve permanecer Rust/Ratatui, source-owned, menor que as referências e guiada
por seus próprios benchmarks. Nenhuma delas autoriza segundo runtime visual ou
novo proprietário do terminal.
