# Evidência e auditorias — índice

Relatórios datados gerados por auditorias, gates e benchmarks. **Não substituem
o código nem os contratos normativos** em `Documentações - Projeto/` — consulte
[RULES.md](../RULES.md) §R5 antes de usar qualquer número como backlog.

## Relatórios canônicos

| Documento | Escopo |
|---|---|
| [SUMMARY.md](SUMMARY.md) | Auditoria histórica da TUI (desatualizada; ver aviso no topo) |
| [RELATORIO-AUDITORIA-ESTATICA-2026-08-30.md](RELATORIO-AUDITORIA-ESTATICA-2026-08-30.md) | Auditoria estática do workspace Rust |
| [OTIMIZACAO-TESTES-E-BUILD-SLIM.md](OTIMIZACAO-TESTES-E-BUILD-SLIM.md) | Otimização da suíte e build (2026-09-04) |
| [REVISAO-VISUAL-TUI-SLIM.md](REVISAO-VISUAL-TUI-SLIM.md) | Revisão visual da TUI (2026-09-04) |
| [HARNESS-SLIM-ETAPA-1.md](HARNESS-SLIM-ETAPA-1.md) … [ETAPA-5](HARNESS-SLIM-ETAPA-5.md) | Cinco etapas do harness Slim |
| [QUICK-WINS-AGILIDADE-NATIVA-SLIM.md](QUICK-WINS-AGILIDADE-NATIVA-SLIM.md) | Quick wins de agilidade nativa |
| [REVISAO-ECONOMIA-TOKENS-NATIVA-SLIM.md](REVISAO-ECONOMIA-TOKENS-NATIVA-SLIM.md) | Economia de tokens nativa |

## Auditorias por data

- `AUDIT-*-2026-09-04.md` — investigações de velocidade, calls, contexto, loops
- `AUDIT-BUGS-2026-09-13/`, `AUDIT-CODE-2026-09-13/` — probes locais (artefatos
  em `probe/` ficam fora do git; ver `.gitignore`)

## O que não versionar

Runs de benchmark, fixtures de probe, capturas ConPTY e logs efêmeros estão no
`.gitignore`. Mantenha apenas relatórios `.md`, scripts de análise e resumos
JSON quando forem evidência permanente.
