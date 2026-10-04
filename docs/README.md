# Documentação do Slim

Este é o índice canônico da documentação versionada. O código define o estado
executável; planos, auditorias, benchmarks e registros datados descrevem o
momento em que foram produzidos.

## Fontes de verdade

| Necessidade | Fonte |
|---|---|
| Regras para agentes | [`../AGENTS.md`](../AGENTS.md) e [`../RULES.md`](../RULES.md) |
| Visão geral e início rápido | [`../README.md`](../README.md) |
| Contrato normativo da TUI | [`DESIGN-SLIM-TUI.md`](DESIGN-SLIM-TUI.md) |
| Decisões de produto | [`DECISOES-GRILL-PRE-IMPLEMENTACAO.md`](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) |
| Arquitetura | [`RUST-CLI.md`](RUST-CLI.md) |
| Plano amplo | [`PLANO-IMPLEMENTACAO.md`](PLANO-IMPLEMENTACAO.md) |
| Fila do ciclo atual | [`PROXIMAS-ETAPAS-AGENTE.md`](PROXIMAS-ETAPAS-AGENTE.md) |
| Referência de CLI/runtime | [`reference/CLI-AND-RUNTIME.md`](reference/CLI-AND-RUNTIME.md) |
| Release/deploy vigente | [`../release/README.md`](../release/README.md) |

## Contratos e planos

- [`DESIGN-SLIM-TUI.md`](DESIGN-SLIM-TUI.md) — contrato visual e funcional.
- [`DECISOES-GRILL-PRE-IMPLEMENTACAO.md`](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) — decisões fechadas e limites.
- [`RUST-CLI.md`](RUST-CLI.md) — mapa de arquitetura e paridade.
- [`PLANO-IMPLEMENTACAO.md`](PLANO-IMPLEMENTACAO.md) — plano e gates históricos.
- [`PROXIMAS-ETAPAS-AGENTE.md`](PROXIMAS-ETAPAS-AGENTE.md) — prioridades do ciclo.
- [`HARNESS-V2-TRACKER.md`](HARNESS-V2-TRACKER.md) — tracker do harness durável.
- [`PROPOSTA-LSP-JS-TS.md`](PROPOSTA-LSP-JS-TS.md) — proposta e implementação nativa de LSP para JavaScript/TypeScript, preservação de Rust e extensão futura para Python.

## Validação e estudos

- [Sugestões do Pi para o Slim (2026-09-22)](../analysis_outputs/2026-09-22-SUGESTOES-PI-PARA-SLIM.md) — propostas por prioridade, quatro altas implementadas e riscos a investigar.
- [`../tests/README.md`](../tests/README.md) — seleção de testes, limites de carga e timings locais.

- [`AUDIT-SLIM-TUI-TRACKER.md`](AUDIT-SLIM-TUI-TRACKER.md) — tracker e evidências da TUI.
- [`VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md`](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md) — POCs e viabilidade.
- [`INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md`](INSIGHTS-MINI-SWE-AGENT-GROK-BUILD.md) — estudo histórico.
- [`../analysis_outputs/README.md`](../analysis_outputs/README.md) — índice de auditorias e evidências datadas.
- [`../poc/README.md`](../poc/README.md) — POCs históricas preservadas.

## Histórico preservado

- [`history/README-2026-09-20.md`](history/README-2026-09-20.md) — antigo README detalhado.
- [`history/PROJECT-INDEX-2026-09-06.md`](history/PROJECT-INDEX-2026-09-06.md) — índice anterior, mantido como registro.
- [`../CHANGELOG.md`](../CHANGELOG.md) — evolução que antes ocupava o topo do README.
- [`../release/history/`](../release/history/) — registros antigos de deploy.

Ao mover ou criar documentação, atualize este índice e valide links relativos.
Não replique contagens, hashes ou estado de deploy em múltiplas fontes vigentes.
