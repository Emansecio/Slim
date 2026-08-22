# POC-0 — toolchain Windows

> **Status de implementação:** POC histórico de pré-requisito; o gate do
> toolchain não prova a integração da v1.

Criar aqui o crate descartável somente quando `rustc`, `cargo` e `cl.exe`
estiverem disponíveis.

Gate: `cargo check` e `cargo build --release` com exit code 0.

No ambiente atual, executar pelo Developer Command Prompt do Visual Studio e
limpar `RUSTC`/`RUSTC_WRAPPER` herdados; o Cargo global referencia `sccache`,
que não está instalado.
