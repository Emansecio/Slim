# POCs históricas

Provas de conceito usadas antes da integração no workspace principal. **Não fazem
parte do build de produção** (`cargo build --workspace` ignora esta pasta).

| POC | Propósito | Como rodar (opcional) |
|---|---|---|
| [search/](search/) | Candidato Rust para busca/indexação | `pwsh -File search/run-poc.ps1` |
| [tui/](tui/) | Probes de renderer e long session | `cargo run --manifest-path tui/Cargo.toml` |
| [toolchain/](toolchain/) | Validação de toolchain | Ver README local |

Artefatos de build (`target/`) estão no `.gitignore`. Preserve apenas fontes e
READMEs; não commitar outputs de compilação.
