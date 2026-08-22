# POC de busca

> **Status de implementação:** POC histórico/componente. Seus resultados não
> provam que o produto completo está integrado.

Comparar `fff-search` com `ripgrep` no Windows após o toolchain estar disponível
para o harness de medição.

Cobertura mínima: primeira busca, warmup, cem buscas repetidas, alterações de
árvore, `.gitignore`, binário e Unicode; registrar p50, p95, RSS e correção.

Harness local: `pwsh -File poc/search/run-poc.ps1`. O harness não remove o
fixture temporário; o caminho criado é reportado para inspeção posterior.

POC do candidato Rust: `cargo run --manifest-path poc/search/Cargo.toml -- <root>`.
Ele mede o `fff-search` real no mesmo fixture do baseline.

RSS do baseline: `pwsh -File poc/search/measure-rg-rss.ps1 -Root <root>`.
