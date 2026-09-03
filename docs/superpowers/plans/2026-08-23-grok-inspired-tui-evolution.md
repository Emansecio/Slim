# Grok-inspired TUI Evolution Implementation Plan

**Status:** concluído em 2026-08-23 — cinco slices implementados, revisados, gate final 63 suítes / 566 passed, deploy `OK:` e limpeza de 22.515 artefatos / 12,4 GiB. Tracker vivo contém desvios e hardening G1–G190.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps usam checkbox (`- [x]`) como registro de execução.

**Goal:** Implementar cinco evoluções sequenciais da TUI Slim — hierarquia Markdown, motion/activity, usage vivo, page-fill scroll/scrollbar e SessionRail — com TDD, revisão independente e gate verde entre slices.

**Architecture:** Reusar reducer, `AppState`, lanes bounded, renderer Ratatui e virtualização atuais. Cada slice altera primeiro o contrato normativo, prova RED, aplica a menor mudança vertical, passa testes/Clippy/benchmark aplicável, recebe revisão independente e só então libera o próximo slice.

**Tech Stack:** Rust 2021, Ratatui 0.29, Crossterm 0.28, `pulldown-cmark` 0.13, `unicode-width`, `proptest`, TestBackend.

---

## 0. Protocolo da árvore existente

Árvore já contém dezenas de mudanças do usuário/agentes. Não executar `reset`, `checkout`, `restore`, `stash`, rebase ou rewrite integral. Antes de editar qualquer arquivo:

```powershell
git diff -- <arquivo>
git status --short -- <arquivo>
```

Aplicar somente hunks relacionados a este plano. Não criar commits: usuário não pediu e a árvore não oferece baseline limpo por slice.

Toolchain manual:

```powershell
$env:RUSTC = 'C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
```

Antes do primeiro RED:

```powershell
cargo test -p slim-tui
```

Esperado no baseline observado em 2026-08-23: processo exit 0. Recontar; não reutilizar números históricos.

---

# Slice 1 — hierarquia Markdown

### Task 1: Revisar contrato normativo de Markdown

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`

- [x] **Step 1: Atualizar headings e streaming Markdown antes do código**

Aplicar estas decisões em §§1.1, 15.2.1, 19.5, 21.1 e 28.3:

```text
H1 → assistant_accent; H2 → heading_accent; H3 → thinking_accent.
Marcadores #/##/### não aparecem no buffer.
Headings > H3 usam heading_accent sem introduzir cor nova.
Listas, emphasis/strong, code inline, quote e link usam tokens semânticos.
Markdown parcial de streaming deve permanecer legível e convergir no frame final.
Medição e render usam a mesma projeção Markdown.
```

Revogar a frase de §19.5 que proíbe distinção cromática por nível. Preservar proibição de colorir parágrafos inteiros.

- [x] **Step 2: Confirmar diff documental**

```powershell
git diff --check -- 'Documentações - Projeto/DESIGN-SLIM-TUI.md' 'Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md'
```

Esperado: exit 0.

### Task 2: Provar renderer Markdown e invalidação de cache em RED

**Files:**
- Create: `crates/slim-tui/tests/markdown_golden.rs`
- Modify: `crates/slim-tui/src/lib.rs`
- Test: `crates/slim-tui/tests/markdown_golden.rs`

- [x] **Step 1: Criar helper TestBackend que lê símbolo e estilo**

Estrutura mínima:

```rust
use ratatui::{backend::TestBackend, style::Color, Terminal};
use slim_tui::{
    api::UiEvent,
    app::AppState,
    reducer::{reduce, Action},
    render::WrapCache,
    runtime::render_frame,
    theme::{Capabilities, ColorDepth},
};

fn render(markdown: &str, width: u16) -> (String, Vec<Color>) {
    let mut state = AppState::new();
    reduce(&mut state, Action::UiEventReceived(UiEvent::AssistantDelta {
        text: markdown.into(),
    }));
    let backend = TestBackend::new(width, 16);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_frame(frame, &state, caps(), &mut WrapCache::default()))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let symbols = buffer.content.iter().map(|cell| cell.symbol()).collect::<String>();
    let colors = buffer.content.iter().map(|cell| cell.fg).collect();
    (symbols, colors)
}
```

`caps()` usa TrueColor e capabilities falsas.

- [x] **Step 2: Adicionar testes comportamentais**

```rust
#[test]
fn headings_hide_markers_and_use_three_semantic_colors() {
    let (symbols, colors) = render("# Primary\n## Structure\n### Detail", 80);
    assert!(!symbols.contains("# Primary"));
    assert!(!symbols.contains("## Structure"));
    assert!(!symbols.contains("### Detail"));
    assert!(symbols.contains("Primary"));
    assert!(symbols.contains("Structure"));
    assert!(symbols.contains("Detail"));
    // localizar primeira célula de cada palavra e exigir três cores distintas
    let heading_colors = colors_for_words(&symbols, &colors, &["Primary", "Structure", "Detail"]);
    assert_eq!(heading_colors.len(), 3);
    assert_ne!(heading_colors[0], heading_colors[1]);
    assert_ne!(heading_colors[1], heading_colors[2]);
}

#[test]
fn inline_markdown_renders_without_control_markers() {
    let (symbols, _) = render(
        "- one\n- **strong** and *emphasis* with `code` and [link](https://example.com)\n> quote",
        80,
    );
    for marker in ["**strong**", "*emphasis*", "`code`", "]("] {
        assert!(!symbols.contains(marker), "raw marker {marker}");
    }
    for text in ["one", "strong", "emphasis", "code", "link", "quote"] {
        assert!(symbols.contains(text));
    }
}

#[test]
fn incomplete_streaming_markdown_stays_visible() {
    let (symbols, _) = render("## Stable\n**unfinished", 40);
    assert!(symbols.contains("Stable"));
    assert!(symbols.contains("unfinished"));
}

#[test]
fn wide_glyphs_do_not_overrun_background_width() {
    let (symbols, _) = render("# 宇宙 🌌 e\u{301}", 40);
    assert!(symbols.contains("宇宙"));
}
```

Adicionar teste unitário em `render.rs`:

```rust
#[test]
fn streaming_delta_invalidates_cached_height() {
    let mut cache = WrapCache::default();
    let mut block = Block::new("assistant-1", BlockKind::Assistant("short".into()), BlockLifecycle::Streaming);
    let first = HeightIndex::build(std::slice::from_ref(&block), 20, &mut cache).total_rows;
    block.append_text("\nsecond row\nthird row");
    let second = HeightIndex::build(std::slice::from_ref(&block), 20, &mut cache).total_rows;
    assert!(second > first);
}
```

- [x] **Step 3: Executar RED**

```powershell
cargo test -p slim-tui --test markdown_golden
cargo test -p slim-tui streaming_delta_invalidates_cached_height
```

Esperado: FAIL por marcadores crus/estilos ausentes e altura stale. Se qualquer teste passar integralmente, ajustar o assert para provar comportamento faltante antes de produzir código.

### Task 3: Implementar parser e geração de conteúdo

**Files:**
- Modify: `crates/slim-tui/Cargo.toml`
- Modify: `Cargo.lock`
- Create: `crates/slim-tui/src/markdown.rs`
- Modify: `crates/slim-tui/src/lib.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/theme.rs`
- Modify: `crates/slim-tui/src/block.rs`
- Modify: `crates/slim-tui/src/render.rs`

- [x] **Step 1: Adicionar única dependência necessária**

```toml
pulldown-cmark = { version = "0.13", default-features = false }
```

Não adicionar `syntect`, `tachyonfx`, snapshot framework ou tokenizer.

- [x] **Step 2: Tornar conteúdo versionável**

Em `Block`:

```rust
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    pub lifecycle: BlockLifecycle,
    pub fold: FoldState,
    pub content_generation: u64,
}

pub fn append_text(&mut self, text: &str) {
    match &mut self.kind {
        BlockKind::Assistant(current) | BlockKind::Thinking(current) => current.push_str(text),
        _ => return,
    }
    self.content_generation = self.content_generation.saturating_add(1);
}
```

`Block::new` inicia geração `0`. Reducer usa `append_text` em Assistant/Thinking e incrementa geração ao trocar preview de tool.

Chave do cache:

```rust
BoundedCache<(String, u64, u16, bool), usize>
```

- [x] **Step 3: Criar projeção Markdown pura**

`markdown.rs` expõe somente:

```rust
pub(crate) struct MarkdownStyles {
    pub text: Style,
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub link: Style,
    pub code: Style,
    pub quote: Style,
    pub strong: Style,
}

pub(crate) fn render_markdown(
    source: &str,
    width: u16,
    styles: MarkdownStyles,
) -> Vec<Line<'static>>;

pub(crate) fn markdown_row_count(source: &str, width: u16) -> usize;
```

Usar `pulldown_cmark::Parser::new_ext` com tabelas/footnotes desativadas; somente features necessárias a headings/listas/emphasis/code/link/quote. Eventos `Start/End/Text/Code/SoftBreak/HardBreak` montam spans. URL fica fora do texto visual; nenhum I/O. Parser panic/error não existe na API, mas saída vazia para fonte não vazia cai para uma linha plain.

Sanitizar C0/ESC antes dos spans:

```rust
fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|ch| matches!(*ch, '\n' | '\t') || !ch.is_control())
        .collect()
}
```

- [x] **Step 4: Mapear tokens existentes**

Adicionar `link_accent` a `Theme` com `#8CB4FF` conforme §21.3 e a `Palette`. Usar:

```rust
MarkdownStyles {
    text: palette.text,
    h1: palette.accent,
    h2: palette.heading,
    h3: palette.thinking,
    link: palette.link,
    code: palette.code_rail,
    quote: palette.muted,
    strong: palette.text.add_modifier(Modifier::BOLD),
}
```

No chrome dos blocos, `Slim` usa `palette.accent` sem bold; `you` permanece `palette.secondary`. Heading strong não altera cor semântica.

- [x] **Step 5: Unificar medição e render**

`runtime::assistant_body` chama `markdown::render_markdown`. `render::block_height` chama `markdown_row_count` para Assistant. User/Thinking padding usa `UnicodeWidthStr::width`, nunca `chars().count()`.

- [x] **Step 6: Executar GREEN**

```powershell
cargo fmt --all
cargo test -p slim-tui --test markdown_golden
cargo test -p slim-tui streaming_delta_invalidates_cached_height
cargo test -p slim-tui
cargo clippy -p slim-tui --all-targets --locked -- -D warnings
```

Esperado: todos exit 0, 0 warnings.

### Task 4: Revisar e fechar Slice 1

- [x] **Step 1: Revisar diff integral do slice**

```powershell
git diff --check
git diff -- crates/slim-tui/Cargo.toml crates/slim-tui/src/markdown.rs crates/slim-tui/src/lib.rs crates/slim-tui/src/runtime.rs crates/slim-tui/src/theme.rs crates/slim-tui/src/block.rs crates/slim-tui/src/render.rs crates/slim-tui/tests/markdown_golden.rs
```

Inspecionar: ANSI/OSC, Unicode width, fonte preservada, cache bounded, nenhuma alocação quadrática óbvia.

- [x] **Step 2: Solicitar revisão independente**

Revisor recebe spec, arquivos do slice e outputs. Bloquear avanço por P0/P1 ou gargalo de hot path. Corrigir achado com novo teste RED quando comportamental; repetir Task 3 Step 6.

- [x] **Step 3: Registrar gate no tracker §7**

Incluir comandos/resultado e marcar G1/G4 resolvidos. Somente então liberar Slice 2.

---

# Slice 2 — motion e ActivityRail

### Task 5: Revisar contrato normativo de motion

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`

- [x] **Step 1: Atualizar §§1.1, 2, 10.3, 15.4, 21.4, 27 e 28.3**

Texto normativo:

```text
Motion ativo: 12 fps nominais (83 ms).
Welcome-only pode renderizar até 12 fps, com fase temporal de 4–6 s.
Sem welcome/run/stream animado: zero tick periódico.
Reduced motion: zero tick ambiente e glyph/caret estático.
Estado transitório corrente vive na ActivityRail; ActivityBlock só representa histórico persistido.
Retrying só aparece por evento real.
```

- [x] **Step 2: `git diff --check` documental**

Esperado: exit 0.

### Task 6: Provar clock, fases e caret em RED

**Files:**
- Create: `crates/slim-tui/tests/motion_activity_golden.rs`
- Modify: tests unitários em `crates/slim-tui/src/welcome.rs` e `runtime.rs`

- [x] **Step 1: Escrever testes**

Contrato de clock:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameClock {
    pub frame: u64,
    pub elapsed_ms: u64,
}
```

Testes:

```rust
#[test]
fn welcome_phase_takes_five_seconds() {
    assert_eq!(welcome::phase(FrameClock { frame: 0, elapsed_ms: 0 }), 0);
    assert_eq!(welcome::phase(FrameClock { frame: 60, elapsed_ms: 5_000 }), 0);
}

#[test]
fn activity_projects_known_phase_and_elapsed() {
    let mut state = AppState::new();
    reduce(&mut state, Action::Tick(FrameClock { frame: 1, elapsed_ms: 1_000 }));
    reduce(&mut state, Action::UiEventReceived(UiEvent::RunStarted));
    reduce(&mut state, Action::UiEventReceived(UiEvent::ThinkingDelta { text: "plan".into() }));
    reduce(&mut state, Action::Tick(FrameClock { frame: 31, elapsed_ms: 3_500 }));
    let frame = render_state(&state, caps(false));
    assert!(frame.contains("Thinking"));
    assert!(frame.contains("2s"));
}

#[test]
fn reduced_motion_removes_caret_without_reflow() {
    let state = streaming_assistant_state("answer");
    let moving = render_state(&state, caps(false));
    let reduced = render_state(&state, caps(true));
    assert!(moving.contains('▌'));
    assert!(!reduced.contains('▌'));
    assert_eq!(moving.lines().count(), reduced.lines().count());
    assert_eq!(moving.lines().map(str::len).collect::<Vec<_>>(), reduced.lines().map(str::len).collect::<Vec<_>>());
}

#[test]
fn activity_updates_do_not_append_transcript_blocks() {
    let mut state = AppState::new();
    for label in ["searching", "waiting"] {
        reduce(&mut state, Action::UiEventReceived(UiEvent::ActivityChanged { label: label.into() }));
    }
    assert!(state.blocks.iter().all(|block| !matches!(block.kind, BlockKind::Activity(_))));
    assert!(matches!(state.activity.as_ref().map(|a| &a.phase), Some(ActivityPhase::External(label)) if label == "waiting"));
}
```

- [x] **Step 2: Executar RED**

```powershell
cargo test -p slim-tui motion_
cargo test -p slim-tui --test motion_activity_golden
```

Esperado: FAIL por `FrameClock`/fase/activity inexistentes e bloco transitório atual.

### Task 7: Implementar clock e fase única

**Files:**
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/welcome.rs`

- [x] **Step 1: Adicionar tipos mínimos**

```rust
pub enum ActivityPhase {
    Thinking,
    Responding,
    RunningTool(String),
    WaitingForInput,
    External(String),
}

pub struct ActivityState {
    pub phase: ActivityPhase,
    pub started_ms: u64,
}
```

`AppState` guarda `clock: FrameClock` e `activity: Option<ActivityState>`. Não guardar `Instant` no estado.

- [x] **Step 2: Projetar lifecycle**

- `RunStarted`: working=true; activity permanece ausente até fase conhecida.
- primeiro `ThinkingDelta`: `Thinking`.
- primeiro `AssistantDelta`: `Responding`.
- `ToolStarted(name)`: `RunningTool(name)`.
- `ToolEnded`: volta a `Responding` somente se run continua.
- `InputRequired`: `WaitingForInput`.
- `InputRequired` deixa de virar `Notification`: `UiEvent::from_core` projeta variante própria.
- `ActivityChanged`: substitui `External(label)`, sem criar bloco.
- completion/cancel/error: limpa activity.
- mudança de fase grava `started_ms = state.clock.elapsed_ms`.

- [x] **Step 3: Scheduler de 83 ms**

Runtime mantém `started_at: Instant`; ao tick:

```rust
let clock = FrameClock {
    frame: frame.saturating_add(1),
    elapsed_ms: started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
};
reduce(&mut state, Action::Tick(clock));
```

`animating = !capabilities.reduced_motion && (state.working || welcome_visible(&state))`. Intervalo único 83 ms. Sem animação, poll não agenda tick.

- [x] **Step 4: Render puro**

Remover `capabilities_reduced_motion()`. Passar `Capabilities` a `render_activity_rail`. Label e elapsed vêm de `state.activity`; unknown usa `Working`, sem inventar retry. Caret ocupa célula reservada ao fim do Assistant streaming.

`render_operational_bar` recebe `activity_visible`: quando a ActivityRail está visível, footer não repete `Working`; quando o layout remove a rail, footer mantém `Working… ^C` como fallback crítico.

Welcome usa `elapsed_ms % 5_000` para fase; paleta/glyph mantém largura 1.

- [x] **Step 5: GREEN e benchmark**

```powershell
cargo fmt --all
cargo test -p slim-tui motion_
cargo test -p slim-tui --test motion_activity_golden
cargo test -p slim-tui
cargo clippy -p slim-tui --all-targets --locked -- -D warnings
cargo bench -p slim-tui --bench long_session
```

Esperado: exit 0; `input_to_frame_p95_ms ≤ 16`.

### Task 8: Revisar e fechar Slice 2

Revisar diff, scheduler, zero-tick idle, reduced motion, alocação por frame e benchmark. Pedir revisão independente; P0/P1 bloqueia. Registrar tracker §7 e marcar G5/G6 resolvidos antes do Slice 3.

---

# Slice 3 — usage/contexto vivo

### Task 9: Revisar contrato normativo de usage

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`
- Modify: `docs/superpowers/specs/2026-08-23-grok-inspired-tui-evolution-design.md`

- [x] **Step 1: Fixar contrato final**

Substituir a forma provisória anterior por:

```rust
UiEvent::UsageEstimate {
    context_tokens: u64,
    context_window_tokens: u64,
}
```

Semântica:

- `ContextSnapshot` core carrega estimativa de input real e window configurada.
- output parcial aumenta estimativa a partir dos chars realmente recebidos.
- `UiEvent::Usage` continua cumulativo/billable e confirmado.
- `AssistantEnded` torna o contexto da última request exato quando usage existe.
- UI mostra `~` enquanto estimate não foi confirmado.

Atualizar §§7.2, 8.2, 10.2, 15.1 e 15.4; remover qualquer exemplo fixo `128k` como regra universal.

### Task 10: Provar ContextSnapshot e estado de estimate em RED

**Files:**
- Modify tests: `crates/slim-core/tests/agent_loop.rs`
- Modify tests: `crates/slim-cli/tests/tui_bridge.rs`
- Modify tests: `crates/slim-tui/tests/m2_integration.rs`

- [x] **Step 1: Testes core**

```rust
#[test]
fn context_snapshot_reports_estimate_and_configured_window() {
    let runtime = run_single_turn_fixture(AgentLoopConfig {
        context_window_tokens: 64_000,
        max_turns: 1,
        ..AgentLoopConfig::default()
    });
    let snapshot = runtime.app.events().iter().find_map(|event| match event.kind {
        EventKind::ContextSnapshot { estimated_tokens, context_window_tokens, .. } => {
            Some((estimated_tokens, context_window_tokens))
        }
        _ => None,
    }).expect("context snapshot");
    assert!(snapshot.0 > 0);
    assert_eq!(snapshot.1, 64_000);
}
```

`run_single_turn_fixture` deve seguir o fixture HTTP local já usado no arquivo: uma request, delta `ok`, usage e `[DONE]`; retorna `Runtime` para inspeção dos eventos.

Compatibilidade serde:

```rust
#[test]
fn legacy_context_snapshot_defaults_new_fields() {
    let event: SessionEvent = serde_json::from_str(
        r#"{"seq":1,"kind":{"type":"ContextSnapshot","tools_bytes":1,"history_bytes":2}}"#,
    ).expect("legacy event");
    // novos campos == 0
}
```

- [x] **Step 2: Testes bridge/TUI**

```rust
#[test]
fn usage_estimate_precedes_stream_completion_and_uses_configured_window() {
    let (runtime, channels) = spawn_fixture_with_options(
        ProviderRunOptions::default().with_context_window_tokens(64_000),
    );
    channels.commands.send(UiCommand::SendPrompt("hello".into())).expect("send");
    let estimate = channels.events_data.recv_timeout(Duration::from_secs(2)).expect("estimate");
    assert!(matches!(estimate, UiEvent::UsageEstimate { context_window_tokens: 64_000, .. }));
    assert_ne!(channels.events.try_recv(), Ok(UiEvent::RunCompleted));
    drop(runtime);
}

#[test]
fn estimate_is_monotonic_and_final_usage_removes_tilde() {
    let mut state = AppState::new();
    for context_tokens in [10, 8] {
        reduce(&mut state, Action::UiEventReceived(UiEvent::UsageEstimate {
            context_tokens,
            context_window_tokens: 100,
        }));
    }
    assert_eq!(state.context_tokens, 10);
    reduce(&mut state, Action::UiEventReceived(UiEvent::Usage { input_tokens: 7, output_tokens: 5 }));
    reduce(&mut state, Action::UiEventReceived(UiEvent::AssistantEnded));
    assert_eq!(state.context_tokens, 12);
    assert!(state.context_exact);
    assert!(!format_context(&state, false).contains('~'));
}
```

- [x] **Step 3: Executar RED**

```powershell
cargo test -p slim-core --test agent_loop context_snapshot_reports_estimate
cargo test -p slim-cli --test tui_bridge usage_estimate
cargo test -p slim-tui --test m2_integration estimate_is_monotonic
```

Esperado: FAIL por campos/evento/estado ausentes.

### Task 11: Implementar estimate sem tokenizer novo

**Files:**
- Modify: `crates/slim-core/src/events.rs`
- Modify: `crates/slim-core/src/context/compact.rs`
- Modify: `crates/slim-core/src/context/mod.rs`
- Modify: `crates/slim-core/src/runtime/mod.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Modify: `crates/slim-cli/src/tui.rs`
- Modify: `crates/slim-cli/src/headless.rs`

- [x] **Step 1: Enriquecer snapshot retrocompatível**

```rust
ContextSnapshot {
    tools_bytes: u64,
    history_bytes: u64,
    #[serde(default)]
    estimated_tokens: u64,
    #[serde(default)]
    context_window_tokens: u64,
}
```

Runtime preenche com `estimated_tokens` já calculado e `config.context_window_tokens`.

- [x] **Step 2: Expor estimador por chars**

```rust
pub fn estimate_text_tokens_from_chars(chars: u64) -> u64 {
    MESSAGE_OVERHEAD_TOKENS.saturating_add(
        chars.saturating_mul(2).div_ceil(TOKENS_PER_ESTIMATED_CHARS_X2 as u64)
    )
}
```

Reusar dentro de `estimate_provider_message_tokens`; não duplicar razão 3,5 na TUI.

- [x] **Step 3: Projetar evento TUI e coalescing**

`UiEvent::from_core(ContextSnapshot)` produz `UsageEstimate`. `is_control=false`. `EventCoalescer::push_data` substitui estimate pendente pelo snapshot de maior `context_tokens` para a mesma window.

- [x] **Step 4: Estado de apresentação**

Adicionar:

```rust
pub context_tokens: u64,
pub context_window_tokens: u64,
pub context_exact: bool,
pub stream_output_chars: u64,
```

- `UsageEstimate`: window válida substitui window; context usa `max`; exact=false.
- `AssistantDelta`: soma chars e recalcula context estimate sobre base do snapshot.
- `Usage`: mantém totais cumulativos atuais e guarda usage da request corrente.
- `AssistantEnded`: se houve usage da request, context=input+output e exact=true.
- run terminal sem usage mantém estimate `~`.

Formatador único `format_context(state, compact)` serve SessionRail, ActivityRail, footer e ViewModel. Durante run, ActivityRail mostra contexto atual à direita; nenhum `128_000` literal permanece.

- [x] **Step 5: Publicar cwd/window reais**

Tornar `resolve_context_window_tokens` `pub(crate)`. `TuiStartup` recebe cwd já obtido por `std::env::current_dir()` no CLI e window resolvida; worker envia `SessionSnapshot { cwd, context_window_tokens }` antes do primeiro run. Render não lê env/filesystem.

- [x] **Step 6: GREEN**

```powershell
cargo fmt --all
cargo test -p slim-core --test agent_loop context_snapshot_reports_estimate
cargo test -p slim-cli --test tui_bridge usage_estimate
cargo test -p slim-tui --test m2_integration estimate_is_monotonic
cargo test -p slim-core
cargo test -p slim-cli
cargo test -p slim-tui
```

Esperado: exit 0, sem warnings.

### Task 12: Revisar e fechar Slice 3

Revisar serde/replay, monotonicidade, overflow, Anthropic parcial, multi-turn tools/compaction, nenhuma telemetria textual e ausência de hardcode. Pedir revisão independente; P0/P1 bloqueia. Atualizar tracker §7 e marcar G3 resolvido.

---

# Slice 4 — page-fill scroll e scrollbar

### Task 13: Revisar contrato normativo de scroll

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`

- [x] **Step 1: Atualizar §§13.3, 15.2.1, 18 e 28**

Fixar:

```text
Novo turno preserva prompt no topo útil enquanto prompt+resposta cabem.
Após overflow, LiveEdge acompanha cauda.
Pinned usa anchor estável (BlockId,row_offset), não distância fixa do fim.
Home vai ao início; End volta ao LiveEdge.
Scrollbar aparece com overflow; drag fica fora deste conjunto de slices.
```

### Task 14: Provar anchor/page-fill em RED

**Files:**
- Modify: `crates/slim-tui/tests/scroll_golden.rs`
- Modify: `crates/slim-tui/tests/properties.rs`
- Modify tests unitários: `crates/slim-tui/src/render.rs`

- [x] **Step 1: Adicionar regressões**

```rust
#[test]
fn sent_prompt_stays_at_top_until_response_overflows() {
    let state = one_turn_state("question", "short answer");
    let frame = render_size(&state, 80, 12);
    let question_row = frame.lines().position(|line| line.contains("question")).expect("question");
    assert!(question_row <= 2, "prompt must stay near top: {question_row}");
    assert!(frame.contains("short answer"));
}

#[test]
fn overflowing_response_switches_to_live_tail() {
    let state = one_turn_state("question", &(0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"));
    let frame = render_size(&state, 80, 12);
    assert!(!frame.contains("question"));
    assert!(frame.contains("line 39"));
}

#[test]
fn pinned_anchor_does_not_move_when_content_arrives() {
    let mut state = overflow_state();
    pin_one_page_up(&mut state, 80, 24);
    let before = transcript_rows(&render_size(&state, 80, 24));
    append_answer(&mut state, "fresh answer");
    let after = transcript_rows(&render_size(&state, 80, 24));
    assert_eq!(before, after, "pinned viewport moved after append");
}

#[test]
fn home_goes_to_first_block() {
    let mut state = overflow_state();
    scroll_with_metrics(&mut state, ScrollIntent::Top, 80, 24);
    assert!(render_size(&state, 80, 24).contains("question number 0"));
}

#[test]
fn scrollbar_appears_only_with_overflow() {
    let short = render_size(&one_turn_state("q", "a"), 80, 24);
    let long = render_size(&overflow_state(), 80, 24);
    assert!(!right_column(&short).contains('┃'));
    assert!(right_column(&long).contains('┃'));
}
```

Property:

```rust
prop_assert!(anchor.row_offset <= measured_block_height);
prop_assert!(viewport_start <= total_rows.saturating_sub(viewport_rows));
```

- [x] **Step 2: Executar RED**

```powershell
cargo test -p slim-tui --test scroll_golden
cargo test -p slim-tui --test properties scroll_
```

Esperado: FAIL em page-fill, anchor estável, Home e scrollbar.

### Task 15: Implementar anchor estável e page-fill

**Files:**
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/reducer.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/theme.rs`

- [x] **Step 1: Substituir estado booleano por enum**

```rust
pub struct ScrollAnchor {
    pub block_id: BlockId,
    pub row_offset: u64,
}

pub enum FollowMode {
    LiveEdge { prompt_id: Option<BlockId> },
    Pinned(ScrollAnchor),
    Top,
}

pub struct ScrollState {
    pub mode: FollowMode,
    pub unseen: u32,
}
```

`UserMessageAdded` em LiveEdge grava novo `prompt_id`. Conteúdo novo em Pinned incrementa unseen; não muda anchor.

- [x] **Step 2: Resolver anchor bidirecionalmente**

Adicionar em `HeightIndex`:

```rust
pub fn row_for_anchor(&self, anchor: &ScrollAnchor) -> Option<u64>;
pub fn anchor_for_row(&self, row: u64) -> Option<ScrollAnchor>;
pub fn prefix_for_block(&self, id: &BlockId) -> Option<u64>;
```

Bloco removido cai para `Top`; row_offset é clampado à altura medida.

- [x] **Step 3: Medir scroll antes do reducer**

Runtime mantém último `ScrollMetrics { viewport_start, viewport_rows, total_rows, top_anchor }` produzido por função pura `measure_scrollback`. Terminal scroll vira `Action::Scroll { intent, metrics }`; reducer calcula próximo anchor sem I/O/render.

Testes TestBackend usam a mesma `measure_scrollback`; nenhuma mutação ocorre dentro de `render_frame`.

- [x] **Step 4: Política page-fill**

Em LiveEdge:

```rust
if let Some(prompt_prefix) = index.prefix_for_block(prompt_id) {
    let rows_since_prompt = index.total_rows.saturating_sub(prompt_prefix);
    if rows_since_prompt <= viewport_rows {
        prompt_prefix
    } else {
        bottom
    }
} else {
    bottom
}
```

- [x] **Step 5: Scrollbar em uma célula**

Adicionar tokens faltantes `scrollbar_track/#151A20` e `scrollbar_thumb/#4B5563`. Quando `total_rows > viewport_rows`, recalcular Markdown/wrap com `area.width - 1`, desenhar transcript nessa largura e track/thumb na coluna final. Thumb mínimo 1:

```rust
let thumb_len = ((viewport_rows * viewport_rows) / total_rows).max(1);
let thumb_top = (start_row * (viewport_rows - thumb_len)) / (total_rows - viewport_rows).max(1);
```

No-color usa `│`/`┃`; live edge usa estilo muted, pinned usa thumb normal. Sem drag.

- [x] **Step 6: GREEN e benchmark**

```powershell
cargo fmt --all
cargo test -p slim-tui --test scroll_golden
cargo test -p slim-tui --test properties scroll_
cargo test -p slim-tui
cargo clippy -p slim-tui --all-targets --locked -- -D warnings
cargo bench -p slim-tui --bench long_session
```

Esperado: exit 0; p95 `≤16 ms`.

### Task 16: Revisar e fechar Slice 4

Revisar off-by-one, resize, wide chars, notice rows, viewport 1, anchor removido e segundo build ao reservar scrollbar. Pedir revisão independente; P0/P1 bloqueia. Atualizar tracker §7 e marcar G2 resolvido.

---

# Slice 5 — SessionRail conversacional

### Task 17: Revisar contrato normativo de layout

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md`

- [x] **Step 1: Resolver todas as contradições de ContextRail**

Atualizar §§1.1, 2, 14.1–14.5, 15.1, 21.4, 28.3 e registros W2/W8:

```text
SessionRail existe somente com transcript não vazio, width≥80 e height≥12.
Welcome e emergência nunca mostram SessionRail.
SessionRail: cwd display-safe à esquerda, contexto atual à direita.
Footer remove contexto total quando rail visível; mantém ↑/↓, cancel e unseen.
Quando rail oculta, footer volta a mostrar contexto compacto.
```

Não reintroduzir branding no topo.

### Task 18: Provar layout em RED

**Files:**
- Modify: `crates/slim-tui/tests/layout_golden.rs`
- Modify: `crates/slim-tui/tests/golden_matrix.rs`
- Modify: `crates/slim-tui/tests/properties.rs`

- [x] **Step 1: Adicionar testes**

```rust
#[test]
fn session_rail_is_conversation_only() {
    let mut welcome = AppState::new();
    welcome.cwd = r"D:\Slim".into();
    assert!(!render_to_string(&welcome, 100, 24).contains(r"D:\Slim"));
    append_answer(&mut welcome, "hello");
    assert!(render_to_string(&welcome, 100, 24).contains(r"D:\Slim"));
}

#[test]
fn hidden_session_rail_moves_context_to_footer() {
    let state = conversation_with_context(9_450, 128_000, false);
    let narrow = render_to_string(&state, 79, 12);
    assert!(narrow.lines().last().expect("footer").contains("9.5k/128k"));
}

#[test]
fn wide_session_rail_does_not_duplicate_context_in_footer() {
    let state = conversation_with_context(9_450, 128_000, true);
    let wide = render_to_string(&state, 100, 24);
    assert_eq!(wide.matches("9.5k/128k").count(), 1);
    assert!(!wide.lines().last().expect("footer").contains("9.5k/128k"));
}

#[test]
fn long_wide_cwd_is_cell_truncated_without_touching_context() {
    let mut state = conversation_with_context(9_450, 128_000, true);
    state.cwd = r"D:\宇宙\uma-pasta-com-nome-muito-longo\Slim".into();
    let wide = render_to_string(&state, 100, 24);
    assert!(wide.lines().next().expect("session rail").contains("9.5k/128k"));
    assert!(wide.lines().all(|line| unicode_width::UnicodeWidthStr::width(line) <= 100));
}
```

Atualizar soma de regiões para incluir `session_rail.height`.

- [x] **Step 2: Executar RED**

```powershell
cargo test -p slim-tui --test layout_golden session_rail
cargo test -p slim-tui --test golden_matrix session_rail
cargo test -p slim-tui --test properties tiny_terminal
```

Esperado: FAIL por região/render inexistentes.

### Task 19: Implementar SessionRail

**Files:**
- Modify: `crates/slim-tui/src/layout.rs`
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Modify: `crates/slim-tui/src/app.rs`
- Modify: `crates/slim-tui/src/api.rs`
- Modify: `crates/slim-cli/src/tui.rs`

- [x] **Step 1: Layout explícito**

Adicionar `session_rail: Rect` ao topo. Alterar planner para receber `show_session_rail: bool`; caller calcula:

```rust
let show_session_rail = !welcome_visible(state) && area.width >= 80 && area.height >= 12;
```

A degradação remove SessionRail antes de ActivityRail/Todo/composer. Emergência usa altura 0.

- [x] **Step 2: Metadata display-safe**

CLI resolve cwd uma vez e envia no `SessionSnapshot`; TUI remove controles restantes defensivamente. Abreviar home somente no CLI, onde ambiente é permitido. Truncar por `unicode_width`, reservando primeiro largura do contexto à direita.

- [x] **Step 3: Render e footer únicos**

`render_session_rail` pinta `surface`, cwd muted à esquerda e `format_context` à direita. `render_operational_bar(..., context_in_session_rail)` remove contexto total quando true; mantém `↑/↓`. `ViewModel::status_line` recebe mesma decisão para não divergir do fullscreen.

- [x] **Step 4: GREEN**

```powershell
cargo fmt --all
cargo test -p slim-tui --test layout_golden session_rail
cargo test -p slim-tui --test golden_matrix session_rail
cargo test -p slim-tui --test properties tiny_terminal
cargo test -p slim-tui
cargo clippy -p slim-tui --all-targets --locked -- -D warnings
```

Esperado: exit 0, sem warnings.

### Task 20: Revisar e fechar Slice 5

Revisar tiling 0–200, truncamento CJK/UNC/root drive, ausência no welcome, contexto sem duplicação e transferência ao footer. Pedir revisão independente; P0/P1 bloqueia. Atualizar tracker §7.

---

# Gate final, documentação e deploy

### Task 21: Atualizar status e números autoritativos

**Files:**
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` §1.1
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` §§5/7
- Modify only when counts are present: `README.md`, `Documentações - Projeto/README.md`, `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`, `release/README.md`

- [x] **Step 1: Rodar gate integral sem filtro**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace -j 1 --no-fail-fast
cargo bench -p slim-tui --bench long_session
git diff --check
```

Todos devem sair 0; benchmark deve imprimir p95 `≤16 ms`. Somar linhas `test result: ok. N passed` nesta execução, incluindo Doc-tests.

- [x] **Step 2: Atualizar documentos com números desta execução**

Buscar toda referência numérica:

```powershell
rg -n "[0-9]+ suítes|[0-9]+ passed|testes|p95|ignored|warnings" README.md 'Documentações - Projeto' release/README.md
```

Atualizar somente claims de estado atual; preservar números explicitamente históricos com data/label.

- [x] **Step 3: Repetir grep pelos números substituídos**

Esperado: nenhuma referência antiga ainda rotulada como atual.

### Task 22: Deploy obrigatório

- [x] **Step 1: Executar build/test/deploy canônico**

```powershell
.\refresh-slim.ps1 -Test
```

Esperado: `OK:`; qualquer falha bloqueia entrega.

- [x] **Step 2: Verificar binário do PATH**

```powershell
& 'C:\Users\User\bin\Slim.exe' --version
Get-FileHash 'C:\Users\User\bin\Slim.exe' -Algorithm SHA256
Get-Item 'C:\Users\User\bin\Slim.exe' | Select-Object LastWriteTime,Length
```

Registrar output real no tracker e resposta final.

- [x] **Step 3: Revisão final independente**

Revisor recebe spec, plano, diff completo e evidência dos gates. Corrigir todo P0/P1 e repetir Task 21 + `refresh-slim.ps1 -Test`. Limitações físicas permanecem explícitas: ConPTY/terminal alternativo só podem ser afirmados se realmente executados.
