# Slim — correções dos dois gargalos confirmados

**Goal:** Eliminar o cliff quadrático da cache de alturas da TUI e a revalidação quadrática por append do JSONL durable, sem alterar semântica visual, recovery ou durabilidade.

**Architecture:** Cache LRU O(1) bounded usando a versão de `lru` já travada no workspace; cache de alturas comporta as duas larguras ativas (`W`/`W-1`). Persistência mantém validação completa no open e usa estado incremental transacional no append: `prepare` puro, write+sync, `commit` infalível.

**Constraints:** Preservar WIP existente, fixtures offline, `RUSTC` Scoop/MSVC 1.97.1, `CARGO_BUILD_JOBS=1`, nenhum commit sem pedido, deploy final obrigatório com `refresh-slim.ps1 -Test`.

---

### Task 1 — Cache de alturas TUI

**Files:** `crates/slim-tui/Cargo.toml`, `Cargo.lock`, `crates/slim-tui/src/cache.rs`, `crates/slim-tui/src/render.rs`, testes focados da TUI.

- [ ] RED determinístico prova trabalho de eviction bounded e retenção das variantes `W`/`W-1`.
- [ ] Trocar scan `min_by_key` por LRU O(1), preservando API, capacidade zero e ordem LRU após reads/updates.
- [ ] Reservar 16.384 entradas apenas para alturas; body cache permanece 8.192.
- [ ] GREEN focado + bench `long_session`; revisar contrato e qualidade separadamente.

### Task 2 — Validação incremental durable

**Files:** `crates/slim-core/src/session/{repository,reducer,attempts,queue,tool_phases,jsonl_repo,memory_repo}.rs` e testes de sessão.

- [ ] RED prova que append não chama validação completa do prefixo e que open chama uma vez.
- [ ] Introduzir `DurableAppendValidator` reconstruído uma vez após validação completa no open.
- [ ] Separar `prepare` sem mutação e `commit` infalível nos estados reducer/lifecycle/attempt/queue/tool.
- [ ] Ordem JSONL: prepare → encode/size → write+sync → commit → `records.push`; poison somente em encode/write/sync conforme contrato atual.
- [ ] Manter torn-tail/separator/preflight, sequência, queue, attempt e tool lifecycle equivalentes.
- [ ] GREEN da matriz de repositório/JSONL/attempts/queue/tool phases; revisar contrato e qualidade separadamente.

### Task 3 — Documentação, gates e deploy

- [ ] Atualizar `AUDIT-SLIM-TUI-TRACKER.md` §5/§7 e `DESIGN-SLIM-TUI.md` §1.1.
- [ ] Sincronizar números de testes em `README.md`, docs README, `PLANO-IMPLEMENTACAO.md`, `release/README.md` e tracker.
- [ ] `cargo fmt --all -- --check`, `cargo test --workspace`, Clippy relevante e `git diff --check`.
- [ ] `./refresh-slim.ps1 -Test` imprime `OK:`; target/PATH têm SHA-256, tamanho e timestamp idênticos.
