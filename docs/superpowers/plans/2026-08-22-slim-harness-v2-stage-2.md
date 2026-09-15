# Slim Durable Harness v2 — Etapa 2 Implementation Plan

> **Retomada deste plano:** revalide as pendências no código e execute somente o escopo autorizado, conforme o `AGENTS.md` vigente. Skills e delegação são escolhidas por necessidade; receitas e resultados da execução original não são obrigações gerais.

**Goal:** Entregar `MemoryRepo` e `JsonlRepo` v2 sob um contrato mínimo comum, com conformance compartilhada e recuperação JSONL segura, sem ligar o v2 ao runtime/writer de produção.

**Architecture:** `DurableRepo` expõe somente header, records, append monotônico e prefixo por sequência. `MemoryRepo` mantém estado tipado em memória; `JsonlRepo` mantém o mesmo estado tipado e persiste header/records em JSONL v2, reutilizando os guardrails de caminho, lock, reparse e quarantine do módulo de sessão. Corrupção de bytes é testada apenas no backend JSONL; o backend em memória não simula filesystem.

**Tech Stack:** Rust 2021, `std`, `serde`, `serde_json`, testes de integração do `slim-core`; nenhuma dependência nova.

---

## Fronteiras congeladas

- `CURRENT_SCHEMA_VERSION` continua `1`; `SessionWriter` permanece o writer ativo.
- O novo código trabalha somente com `DurableSessionHeader` e `DurableRecord`.
- Sequência é estritamente crescente (`next > last`), mas gaps são válidos.
- Erros comuns usam `std::io::ErrorKind`; testes não dependem de texto específico do Windows.
- A conformance comum cobre estado tipado, ordem e prefixo lógico. Reopen/recuperação, torn-tail, quarantine, newline, schema inválido, lock e reparse são específicos do `JsonlRepo`, porque o backend em memória não possui bytes a recuperar.
- Etapa 3+ permanece fora do escopo: sem reducer, restore de estado derivado, Effects, retry, tool replay ou wiring no runtime.

### Task 1: Contrato comum e tracer bullet do MemoryRepo

**Files:**
- Create: `crates/slim-core/src/session/repository.rs`
- Create: `crates/slim-core/src/session/memory_repo.rs`
- Modify: `crates/slim-core/src/session/mod.rs`
- Create: `crates/slim-core/tests/session_repo_conformance.rs`

- [x] **Step 1: Escrever o primeiro teste RED pela API pública**

Criar uma fixture de header v2 e provar criação, append e leitura:

```rust
let header = durable_header("memory-session");
let mut repo = MemoryRepo::new(header.clone());
let record = durable_entry(1);
repo.append(record.clone()).expect("append");
assert_eq!(repo.header(), &header);
assert_eq!(repo.records(), &[record]);
```

- [x] **Step 2: Rodar RED**

```powershell
cargo test -p slim-core --test session_repo_conformance memory_repo_appends_and_reads
```

Expected: `EXIT=101`, porque `DurableRepo`/`MemoryRepo` ainda não existem.

- [x] **Step 3: Implementar o contrato mínimo e MemoryRepo**

`repository.rs`:

```rust
pub trait DurableRepo {
    fn header(&self) -> &DurableSessionHeader;
    fn records(&self) -> &[DurableRecord];
    fn append(&mut self, record: DurableRecord) -> io::Result<()>;

    fn read_prefix(&self, through_seq: u64) -> Vec<DurableRecord> {
        self.records()
            .iter()
            .take_while(|record| record.seq() <= through_seq)
            .cloned()
            .collect()
    }
}

pub(crate) fn validate_next(records: &[DurableRecord], next: u64) -> io::Result<()> {
    if records.last().is_some_and(|last| next <= last.seq()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable record sequence must increase",
        ));
    }
    Ok(())
}
```

`memory_repo.rs` mantém apenas `header` e `Vec<DurableRecord>`; `append` valida antes de mutar.

- [x] **Step 4: Rodar GREEN e refatorar somente em GREEN**

```powershell
cargo test -p slim-core --test session_repo_conformance memory_repo_appends_and_reads
```

Expected: `EXIT=0`.

### Task 2: Conformance comum completa no MemoryRepo

**Files:**
- Modify: `crates/slim-core/tests/session_repo_conformance.rs`
- Modify only if required by a RED: `crates/slim-core/src/session/repository.rs`
- Modify only if required by a RED: `crates/slim-core/src/session/memory_repo.rs`

- [x] **Step 1: Adicionar um caso por ciclo RED→GREEN**

A função genérica recebe `R: DurableRepo` e executa, nesta ordem:

```rust
fn assert_common_contract<R: DurableRepo>(mut repo: R) {
    // append dos quatro DurableRecord kinds e igualdade estrutural;
    // read_prefix(2), limite acima do último e repositório vazio;
    // seq duplicada e regressiva => InvalidInput, sem mutação;
    // gap crescente aceito;
    // chamada de leitura repetida não altera estado.
}
```

Cada comportamento novo deve falhar antes da implementação correspondente. Não escrever toda a matriz antes do primeiro GREEN.

- [x] **Step 2: Fechar o backend em memória**

```powershell
cargo test -p slim-core --test session_repo_conformance memory_repo_conformance
```

Expected: `EXIT=0`; nenhum mock de helper interno.

### Task 3: JsonlRepo v2 e mesma conformance

**Files:**
- Create: `crates/slim-core/src/session/jsonl_repo.rs`
- Modify: `crates/slim-core/src/session/event_log.rs`
- Modify: `crates/slim-core/src/session/recovery.rs`
- Modify: `crates/slim-core/src/session/schema_v2.rs`
- Modify: `crates/slim-core/src/session/inspection.rs`
- Modify: `crates/slim-core/src/session/mod.rs`
- Modify: `crates/slim-core/tests/session_repo_conformance.rs`
- Create: `crates/slim-core/tests/session_jsonl_repo.rs`

- [x] **Step 1: Adicionar o RED de criação/reopen**

```rust
let path = temp_path("durable-v2.jsonl");
let header = durable_header("jsonl-session");
let mut repo = JsonlRepo::create(&path, header.clone()).expect("create");
repo.append(durable_entry(1)).expect("append");
drop(repo);
let reopened = JsonlRepo::open(&path).expect("open");
assert_eq!(reopened.header(), &header);
assert_eq!(reopened.records().len(), 1);
```

- [x] **Step 2: Rodar RED**

```powershell
cargo test -p slim-core --test session_repo_conformance jsonl_repo_conformance
```

Expected: `EXIT=101`, porque `JsonlRepo` ainda não existe.

- [x] **Step 3: Implementar wrapper v2 tipado**

`JsonlRepo` deve manter:

```rust
pub struct JsonlRepo {
    path: PathBuf,
    file: File,
    _lock: File,
    header: DurableSessionHeader,
    records: Vec<DurableRecord>,
}
```

`create` resolve o caminho canônico, adquire o lock e grava somente header v2 em arquivo novo/vazio. `open` adquire o mesmo lock antes da leitura, aceita somente header v2, preserva a ordem dos records, deriva o alvo canônico a partir do handle no Windows, repara apenas EOF sintaticamente incompleto ou newline final ausente e então abre append. `append` valida antes de escrever/sincronizar e só depois atualiza o vetor.

Os helpers de filesystem podem ser promovidos para `pub(crate)` ou movidos para um módulo interno comum; não criar `SessionWriter<T>` e não duplicar as checagens de reparse.

- [x] **Step 4: Rodar a mesma conformance contra JsonlRepo**

```powershell
cargo test -p slim-core --test session_repo_conformance
```

Expected: MemoryRepo e JsonlRepo GREEN sob os mesmos asserts comuns.

- [x] **Step 5: Adicionar regressões JSONL específicas, uma por ciclo**

`session_jsonl_repo.rs` deve provar:

1. bytes/golden de header e quatro kinds;
2. reopen preserva records e permite próximo append;
3. torn-tail final é quarantined com bytes exatos e truncado antes do append;
4. JSON completo inválido/interior falha `InvalidData` sem quarantine/mutação;
5. linha válida sem newline recebe separador;
6. colisão de quarantine preserva evidência anterior;
7. schema v1/99 e header/record fora de ordem falham fechados;
8. segundo writer, alias de sessão e sentinel reparse não contornam o lock no Windows.

- [x] **Step 6: Fechar os testes focados**

```powershell
cargo test -p slim-core --lib --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2
```

Resultado: `EXIT=0; 37 passed / 0 failed`.

### Task 4: Revisões, documentação e gate de entrega

**Files:**
- Modify: `Documentações - Projeto/HARNESS-V2-TRACKER.md`
- Modify: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`
- Modify: `Documentações - Projeto/DESIGN-SLIM-TUI.md` only if behavior/status wording changes
- Modify every Markdown file that cites the canonical test count
- Modify: this plan, marking only completed checks

- [x] **Step 1: Fazer revisão de especificação e depois revisão de qualidade**

Spec review confirmou contrato comum, corrupção específica do backend, writer v1 ativo e ausência de Etapa 3+. Quality review foi APROVADA sem P0/P1/P2; H2-B1 permanece aberto fora do escopo. Foram verificados corrupção parcial, lock bypass, TOCTOU, mutation-before-validation e API desnecessária.

- [x] **Step 2: Formatar somente arquivos Rust tocados**

```powershell
rustfmt --edition 2021 <arquivos Rust tocados>
git diff --check -- <arquivos Rust tocados>
```

- [x] **Step 3: Rodar e contar o workspace atual**

```powershell
cargo test --workspace
```

Resultado: gate integral histórico da Etapa 2; contagem supersedida pelo gate da Etapa 3.

- [x] **Step 4: Atualizar tracker e números a partir dessa evidência**

Etapa 2 marcada completa: ambos os backends passam a conformance e as regressões específicas do JSONL estão verdes. RED/GREEN e os limites foram registrados no tracker: v2 ainda não ligado ao runtime/writer; corrupção de bytes não é simulada pelo MemoryRepo; o próximo estado é Etapa 3 em andamento.

- [x] **Step 5: Implantar o binário real**

```powershell
.\refresh-slim.ps1 -Test
```

Resultado: `OK: Slim slim 0.1.0 implantado...`; smoke `slim 0.1.0`, `EXIT=0`; SHA-256 `7fa7fb829405e8c96d4ebd60f6fa32bf6968a3429a59796405428c081aec6b5c`; 6.678.528 B; last write `2026-08-22 20:37:32`.

- [x] **Step 6: Autorrevisão final**

Confirmado: nenhum `Cargo.toml` alterado; `CURRENT_SCHEMA_VERSION == 1`; nenhum wiring em runtime/CLI/TUI; alterações preexistentes da Onda 1 preservadas; H2-B1 continua aberto fora do escopo; grep das contagens canônicas antigas vazio.
