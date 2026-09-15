# Slim Transcript + Inspector Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Transformar o transcript atual em workspace adaptativo com conversa dominante, inspector operacional e hierarquia visual clara.

**Architecture:** Manter `AppState`, protocolo e folding existentes. Alterar somente projeção e renderização: `runtime.rs` calcula workspace central e reserva largura do inspector; `view_model.rs` projeta header de sessão; `render.rs` mantém medição idêntica ao conteúdo; `theme.rs` separa foco ciano de sucesso verde.

**Tech Stack:** Rust 2021, Ratatui 0.29, Crossterm 0.28, testes golden existentes.

**Spec:** `C:\Users\User\Pictures\Screenshots\Captura de tela 2026-08-30 192753.png` + direção aprovada nesta sessão (`transcript + inspector`, hierarquia e polimento visual).

## Global Constraints

- Implementar no workspace atual; preservar alterações não relacionadas.
- Sem nova dependência, framework, protocolo ou estado legado paralelo.
- Reusar folding atual de Thinking e grupos de tools.
- Inspector padrão somente em transcript largo (`>=140` colunas); inspector solicitado continua responsivo.
- Larguras menores preservam coluna única ou overlay já existente.
- Transcript usa aproximadamente 92 células no layout largo; Markdown/código preservam regras atuais dentro dessa largura.
- Vermelho identifica marcador de falha, não colore mensagem inteira.
- Não criar commits no workspace compartilhado sem pedido explícito.

---

### Task 1: Fixar contrato visual em testes

**Files:**
- Modify: `crates/slim-tui/tests/inspectors_golden.rs`
- Modify: `crates/slim-tui/tests/layout_golden.rs`
- Modify: `crates/slim-tui/tests/golden_matrix.rs`

**Interfaces:**
- Consumes: `render_frame(&mut Frame, &AppState, Capabilities, &mut WrapCache)`
- Produces: contrato observável para header, workspace largo, fallback estreito e hierarquia de papéis.

- [ ] **Step 1: Escrever teste principal que falha**

Adicionar fixture com cwd, contexto, run ativo, user, thinking, resposta longa e tools. Em `160x30`, exigir:

```rust
assert!(wide.lines().next().unwrap_or_default().contains("SLIM"));
assert!(wide.lines().next().unwrap_or_default().contains("RUNNING"));
assert!(wide.contains("Run"));
assert!(wide.contains("Slim"));
assert!(wide.contains("assistant-tail-marker"));
```

Falha que captura: remover reserva lateral faz cauda ser apagada pelo inspector; remover header/role label elimina hierarquia.

- [ ] **Step 2: Escrever fallback estreito que falha**

Em `99x24`, exigir transcript utilizável e ausência do inspector padrão:

```rust
assert!(!narrow.contains(" RUN "));
assert!(narrow.contains("assistant-tail-marker"));
```

Falha que captura: inspector padrão virar overlay permanente em terminal estreito.

- [ ] **Step 3: Atualizar expectativas antigas deliberadamente superseded**

Trocar testes que exigiam ausência permanente de `SLIM` e ocultação da SessionRail com cwd trivial por expectativas do novo header conversacional. Preservar unicidade do contexto.

- [ ] **Step 4: Verificar RED**

Run:

```bash
env -u RUSTC cargo test -p slim-tui --test inspectors_golden --test layout_golden --test golden_matrix
```

Expected: falhas específicas por `SLIM`/`RUNNING`/`Run` ausentes ou cauda escondida; nenhuma falha de compilação.

---

### Task 2: Implementar workspace adaptativo e hierarquia

**Files:**
- Modify: `crates/slim-tui/src/runtime.rs`
- Modify: `crates/slim-tui/src/view_model.rs`
- Modify: `crates/slim-tui/src/render.rs`
- Modify: `crates/slim-tui/src/theme.rs`

**Interfaces:**
- Consumes: `AppState`, `LayoutRegions.scrollback`, `InspectorState.active`, `HeightIndex`.
- Produces: `workspace_regions(...)`, header compartilhado e render consistente com medição.

- [ ] **Step 1: Projetar header compartilhado**

Em `view_model.rs`, substituir projeção cwd/context por identidade/status/context:

```text
SLIM · D:\Slim                         RUNNING · Thinking · 18s · ctx ~19%
```

Cwd trivial é omitido, não remove header. Prioridade de truncamento: cwd, fase extensa, elapsed; estado e contexto permanecem.

- [ ] **Step 2: Reservar workspace largo**

Em `runtime.rs`, calcular área central máxima de 144 células. Quando `width >= 140` e há transcript, reservar inspector de 34–52 células; quando inspector explícito está ativo em `width >= 100`, também reservar largura. Abaixo de 100, manter overlay central atual.

`measure_scrollback` deve usar exatamente a largura do transcript calculada pelo mesmo helper.

- [ ] **Step 3: Renderizar inspector operacional padrão**

Quando nenhum inspector explícito estiver ativo, mostrar `Run` com dados existentes:

```text
Status   RUNNING
Phase    Thinking
Elapsed  18s
Tools    13 ok · 1 failed
Errors   1
Context  ctx ~19%
```

Exibir somente atalhos reais (`Ctrl+J`, `Ctrl+D`, `Ctrl+G`, `Ctrl+R`, `Ctrl+C`).

- [ ] **Step 4: Reforçar papéis e largura de leitura**

Adicionar label `Slim` antes de cada bloco assistant e somar uma row em `block_height`. Manter transcript sem inspector limitado a 96 células e transcript + inspector em aproximadamente 92, sem alterar parser Markdown.

- [ ] **Step 5: Reduzir alarme visual**

Falha usa glyph vermelho e texto neutro. Trocar foco/borda ativa para ciano; manter `success` verde e `warning` âmbar.

- [ ] **Step 6: Verificar GREEN focado**

Run:

```bash
env -u RUSTC cargo test -p slim-tui --test inspectors_golden --test layout_golden --test golden_matrix --test thinking_expansion_golden --test multi_tool_golden
```

Expected: todos verdes.

---

### Task 3: Verificar regressões da TUI

**Files:**
- Verify only: `crates/slim-tui/**`

**Interfaces:**
- Consumes: implementação completa.
- Produces: evidência de formato, lint, testes e diff restrito.

- [ ] **Step 1: Formatar**

```bash
env -u RUSTC cargo fmt --all -- --check
```

- [ ] **Step 2: Rodar crate completo**

```bash
env -u RUSTC cargo test -p slim-tui --all-targets
```

- [ ] **Step 3: Rodar Clippy**

```bash
env -u RUSTC cargo clippy -p slim-tui --all-targets --locked -- -D warnings
```

- [ ] **Step 4: Revisar diff**

```bash
git diff --check
git diff -- crates/slim-tui/src/runtime.rs crates/slim-tui/src/view_model.rs crates/slim-tui/src/render.rs crates/slim-tui/src/theme.rs crates/slim-tui/tests/inspectors_golden.rs crates/slim-tui/tests/layout_golden.rs crates/slim-tui/tests/golden_matrix.rs
```

Expected: sem whitespace errors, debug code ou arquivos fora do escopo.
