# Evolução Grok-inspired da TUI do Slim

**Data:** 2026-08-23  
**Status:** implementado, revisado, testado e implantado em 2026-08-23  
**Referência:** mock Grok Build fornecido pelo usuário e código upstream inspecionado  
**Estratégia:** cinco slices verticais sequenciais, cada um bloqueado por teste, benchmark aplicável e revisão sem achados críticos/importantes  
**Evidência final:** 63 suítes / 566 passed / 0 failed / 1 ConPTY ignorado; `refresh-slim.ps1 -Test` imprimiu `OK:`; binário PATH SHA-256 `702492EC9857378137D70CA3A2222D569F0BF73E85AFF51C5EFAAA9E20EE145B`; `cargo clean` removeu 22.515 arquivos / 12,4 GiB

## 1. Objetivo

Aproximar gradualmente a TUI do Slim da linguagem visual e operacional do Grok Build sem reescrever a fundação existente, criar card soup, simular digitação ou sacrificar reduced motion, backpressure e virtualização.

Resultado esperado:

- respostas com hierarquia Markdown clara e sem marcadores visuais crus;
- atividade e streaming perceptivelmente fluidos;
- contexto/tokens atualizados durante o turno, com exatidão explicitada;
- scroll que preserva a pergunta enquanto a resposta começa e respeita navegação manual;
- rail superior de sessão durante conversas, ausente no welcome e em layouts restritos.

## 2. Abordagens consideradas

### A. Evolução vertical incremental — escolhida

Reusar reducer, `AppState`, lanes, `FrameClock`, theme e pipeline virtualizado. Entregar uma capacidade observável por slice e validar antes da próxima.

**Vantagens:** menor risco, regressões localizadas, comparação visual progressiva e rollback simples.  
**Custo:** alguns contratos serão alterados em etapas, especialmente usage e viewport.

### B. Port literal do renderer Grok — rejeitada

Copiar layout, tracker ACP e scrollback do upstream.

**Motivo:** arquiteturas e protocolos diferentes; criaria duplicação, dependências e duas autoridades de estado.

### C. Ajuste cosmético único — rejeitada

Mudar somente cores, labels e FPS.

**Motivo:** não resolve usage final-only, atividade genérica nem comportamento de scroll.

## 3. Restrições permanentes

- Windows-only, Ratatui 0.29 e Crossterm 0.28.
- Reducer permanece única rota de mutação visual.
- Render permanece puro e sem relógio/I/O global.
- Nenhum timer periódico quando não há estado animado.
- Reduced motion congela spinner/caret e remove motion ambiente sem mudar conteúdo ou layout.
- Primeiro delta continua imediato; não existe efeito de máquina de escrever.
- Deltas podem ser coalescidos pelo scheduler, mas lifecycle/completion/error nunca.
- Usage estimado sempre recebe prefixo `~`; somente metadata do provider é exata.
- Nenhuma informação de contexto é duplicada simultaneamente em rail superior e footer.
- Nenhuma nova abstração sem segundo consumidor real.

## 4. Slice 1 — hierarquia Markdown e metadata

### 4.1 Comportamento

- `#`, `##` e `###` deixam de aparecer como texto visual.
- H1 usa `assistant_accent`; H2 usa `heading_accent`; H3 usa `thinking_accent`. Nenhum token novo é necessário.
- Corpo usa `text`; metadata usa `muted`; labels de papel usam `secondary_text`/accent sem bold indiscriminado.
- Listas, ênfase, code inline, citações e links recebem estilo semântico.
- Markdown incompleto durante streaming degrada para texto legível e converge para o frame final correto.
- Timestamp permanece oculto abaixo de 100 células e alinhado à direita quando houver metadata temporal real. A TUI não inventa horário.

### 4.2 Implementação

Preferir `pulldown-cmark`, já previsto pela especificação normativa, porque parser ad-hoc para nesting, escaping e streaming seria maior e mais frágil. Sanitização de controles acontece antes da materialização de spans. O renderer preserva texto fonte para copy.

Arquivos esperados:

- `crates/slim-tui/Cargo.toml`
- `crates/slim-tui/src/runtime.rs`
- `crates/slim-tui/src/theme.rs`
- módulo Markdown pequeno somente se reduzir `runtime.rs`
- goldens e unit tests do `slim-tui`

### 4.3 Gate

1. testes RED provando remoção dos marcadores e cores distintas;
2. testes focados verdes;
3. suíte `slim-tui` verde, sem warnings;
4. revisão de diff sem bug, regressão de width/Unicode ou gargalo evidente;
5. tracker atualizado antes do Slice 2.

## 5. Slice 2 — motion e ActivityRail

### 5.1 Comportamento

- Motion ativo usa intervalo nominal de 83 ms (12 fps), dentro do teto normativo revisado.
- Welcome animado usa a mesma cadência de frame, mas fase baseada em tempo completa uma respiração em 4–6 segundos; render FPS não controla velocidade semântica.
- Sem welcome, run ou streaming animado, não existe tick periódico.
- ActivityRail identifica `Thinking`, `Responding`, `Running <tool>`, `Waiting for input` e `Retrying` quando esses estados forem conhecidos.
- Rail mostra elapsed da fase e cancelamento acionável.
- Assistant streaming mostra caret discreto; reduced motion preserva estado textual e remove caret/pulso.

### 5.2 Mudança normativa

A regra antiga de ambient idle `≤2 fps` passa a permitir **welcome-only até 12 fps**, condicionado a:

- fase lenta de 4–6 s;
- zero ticks sob reduced motion;
- zero ticks quando welcome não está visível;
- degradação prioritária sob backpressure.

Demais idle permanece sem timer.

### 5.3 Gate

- testes determinísticos com clock injetado;
- reduced motion sem mudança de layout;
- suíte `slim-tui` e Clippy focado verdes;
- benchmark long-session p95 `≤16 ms` e inspeção de alocações óbvias no hot path;
- revisão sem achado crítico/importante antes do Slice 3.

## 6. Slice 3 — usage/contexto vivo

### 6.1 Contrato

Preservar `UiEvent::Usage`, que continua representando usage confirmado/billable do provider, e fixar a projeção final de contexto vivo:

```rust
UiEvent::UsageEstimate {
    context_tokens: u64,
    context_window_tokens: u64,
}
```

`UsageEstimate` é snapshot monotônico da request ativa, não delta acumulável. Ele nasce de `ContextSnapshot { estimated_tokens, context_window_tokens }`; campos novos usam `serde(default)` para replay legado. `UiEvent::Usage` continua sendo a única fonte que consolida valores confirmados em `AppState.input_tokens`/`output_tokens`. O estado mantém contexto estimado separado dos totais billable.

### 6.2 Regras

- Metadata parcial confiável continua chegando como `UiEvent::Usage`; estimativa nunca a substitui.
- `UsageEstimate` atrasado nunca reduz a estimativa corrente nem qualquer contador confirmado.
- Estimativa usa helper já existente no core; nenhum tokenizer novo entra apenas para UI.
- Valor provisional aparece com `~`.
- Usage final exato consolida contadores billable; `AssistantEnded` confirma o contexto da request quando usage estiver disponível.
- Provider sem usage parcial mantém contexto estimado a partir do texto realmente enviado/recebido; isso não bloqueia streaming.
- Atualização percorre provider/runtime/CLI/TUI por evento, nunca por leitura direta da TUI.

### 6.3 Gate

- RED/GREEN para parcial, final, replay regressivo e provider sem metadata;
- testes de bridge garantindo ordering com delta final;
- suíte focada `slim-core`, `slim-cli` e `slim-tui` verde;
- revisão de segurança para não expor prompt/output em telemetria;
- revisão sem achado crítico/importante antes do Slice 4.

## 7. Slice 4 — page-fill scroll e scrollbar

### 7.1 Comportamento

Após envio:

1. prompt enviado fica ancorado próximo ao topo útil;
2. resposta preenche espaço abaixo;
3. ao ultrapassar viewport, follow migra para cauda;
4. scroll manual desliga follow e preserva anchor;
5. conteúdo novo incrementa unseen enquanto pinned;
6. `End` e gesto explícito no fundo retomam live edge.

Scrollbar:

- ocupa uma célula da largura útil somente com overflow;
- fica visível ao scroll/pinned/foco e discreta no live edge;
- thumb deriva de offset/viewport/altura total;
- no-color preserva posição por glyph, não só cor;
- drag fica fora deste conjunto de slices; wheel e teclado continuam autoridades.

### 7.2 Estado

Estender o estado existente apenas com anchor/reserva mínima necessária. Não criar segundo modelo de scroll nem copiar `ScrollbackState` do Grok.

### 7.3 Gate

- properties para offset válido, pin, unseen e retorno ao live edge;
- goldens de prompt preservado, overflow e scrollbar;
- benchmark long-session p95 `≤16 ms`;
- revisão de off-by-one, Unicode width e resize;
- revisão sem achado crítico/importante antes do Slice 5.

## 8. Slice 5 — rail superior de sessão

### 8.1 Comportamento

Durante conversa normal:

```text
D:\Slim                                         9.45k / 128k
```

- cwd display-safe à esquerda, com home abreviável;
- usage/contexto à direita;
- ausente no welcome;
- oculto em altura/largura restrita;
- quando oculto, footer recebe representação compacta de contexto;
- quando visível, footer mantém shortcuts, input/output, cancelamento e unseen sem repetir contexto total.

### 8.2 Mudança normativa

Revoga a decisão W2/W8 de ausência absoluta de `ContextRail`. Nova regra: `SessionRail` conversacional e adaptativa; nunca branding permanente e nunca row redundante.

### 8.3 Gate

- goldens welcome/conversa/wide/narrow/emergência;
- layout tiling sem área negativa;
- truncamento por largura de célula;
- suíte `slim-tui` verde;
- revisão sem achado crítico/importante.

## 9. Processo obrigatório entre slices

Cada slice segue, sem exceção:

1. atualizar primeiro o contrato normativo aplicável em `DESIGN-SLIM-TUI.md`;
2. escrever teste observável;
3. executar e confirmar RED pelo motivo esperado;
4. implementar mínimo necessário;
5. executar focused GREEN;
6. rodar suíte afetada, Clippy e benchmark quando aplicável;
7. revisar diff integral e pedir revisão independente;
8. registrar bug novo no tracker §5 antes de corrigi-lo;
9. corrigir todo achado crítico/importante e repetir gates;
10. registrar slice no tracker §7;
11. somente então abrir próximo slice.

Achado menor que não afeta correção, segurança, performance, acessibilidade terminal ou contrato pode ser registrado como dívida explícita; achado crítico/importante bloqueia avanço.

## 10. Gate final e deploy

Após Slice 5:

- `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`;
- `cargo test --workspace -j 1 --no-fail-fast`;
- benchmark `long_session` dentro do budget;
- `git diff --check`;
- atualizar checkpoint §1.1, tracker §7 e todos os números de testes citados;
- grep pelos números antigos;
- executar `./refresh-slim.ps1 -Test` e exigir `OK:`;
- verificar `C:\Users\User\bin\Slim.exe --version` e hash/mtime do binário implantado.

## 11. Não objetivos

- copiar arquitetura ACP ou scrollback completo do Grok;
- introduzir 30/60 fps;
- typewriter artificial;
- syntax highlighting completo neste conjunto de slices;
- drag de scrollbar sem hit regions confiáveis;
- nova framework de UI, sidecar JavaScript ou plugin system;
- mudar provider protocol somente para imitar telemetria inexistente.

## 12. Critérios de aceitação globais

- cada recomendação está observável no binário normal;
- nenhum slice avança com teste falho, warning ou achado crítico/importante aberto;
- first-delta, cancelamento, draft, reduced motion e pinned/unseen existentes não regridem;
- render p95 permanece `≤16 ms` no corpus normativo;
- usage desconhecido/estimado nunca é apresentado como exato;
- layout continua funcional de `40×8` até `200×40` e entra em emergência abaixo disso;
- binário do PATH é atualizado após a última mudança de código.
