# Auditoria operacional de uso do Slim

Data: 2026-08-26  
Estado: **verde nos fluxos locais/offline; validação live condicionada**

## Resultado

A auditoria confirmou e corrigiu três defeitos no startup de autenticação da
TUI e um defeito de accounting no OpenCode Go. Depois das correções, a
reauditoria dos fluxos de uso, o soak de 50 turnos,
os benchmarks locais e o gate integral não produziram novo achado confirmado.

## Achados corrigidos

| ID | Impacto | Evidência e correção |
|---|---|---|
| G295 | alto | `prepare_tui` não reconstruía request a partir do `active_provider` API-key salvo. Startup agora restaura OpenAI-compatible, Anthropic API, OpenCode Go, ClinePass e Command Code. |
| G296 | alto | `resolve_provider_credential` podia devolver OAuth ao construtor API-key, removendo `oauth_session`/refresh. `api_key_request` recusa credencial OAuth e o fluxo OAuth preserva a sessão para Anthropic e Codex. |
| G297 | médio | Falhas do auth store eram convertidas em ausência de credencial. Agora retornam ExitCode Auth explícito; resolução lazy preserva a precedência de env sobre arquivo inferior inválido. |
| G298 | alto | OpenCode Go Chat Completions publicava snapshots progressivos de usage e o segundo abortava a resposta. O transporte consolida o máximo por componente em um único terminal; outros providers continuam estritos. |

Implementação: `crates/slim-cli/src/tui.rs:101`, `:353` e `:385`.  
Regressões: `crates/slim-cli/src/tui.rs:3183`, `:3246`, `:3337` e `:3385`.
G298: `crates/slim-core/src/provider.rs:1148`; regressão end-to-end em
`crates/slim-core/tests/provider_http.rs:709`.

## Matriz operacional revalidada

| Jornada | Evidência local | Resultado |
|---|---|---|
| Startup sem provider explícito | matriz de cinco providers API-key + dois OAuth | verde |
| Startup com provider explícito | Anthropic/Codex OAuth e credenciais API-key | verde |
| Auth inválido e precedência | erro explícito + env sobre auth file inválido | verde |
| Login/store/logout | `auth_security` (18) + `oauth_contract` (12) | verde |
| Streaming e primeiro delta | `tui_bridge`, fixtures SSE localhost | verde |
| Tools e continuidade do turno | `provider_cli`, `agent_loop`, `tui_runtime` | verde |
| Cancelamento/timeout/backpressure | `runtime_abort`, `tui_bridge`, `fault_injection` | verde |
| Compaction e histórico seguinte | `compaction` + round-trip TUI | verde |
| Erros de provider/limites | `provider_http`, contratos headless/TUI | verde |
| Lifecycle/restauração terminal | testes TUI e PTY sintético | verde; ConPTY físico permanece ignorado |

## Soak e desempenho

- Soak TUI offline: 50 turnos, p95 **3,327 ms**, total **137,561 ms**,
  último request **10.535 bytes**, sem stall nem perda de histórico.
- Working set do soak: +9.273.344 bytes enquanto 100 mensagens permanecem no
  histórico; isto é medição, não evidência de leak.
- Seletor de compaction: 10k p95 **1,567 ms**; 20k p95 **2,865 ms**;
  escala **1,83x**.
- TUI com 3.200 blocos/4.968.694 bytes: scroll p95 **0,891 ms** e
  input→frame p95 **1,239 ms**, abaixo do budget de 16 ms.
- Setup de seis tools: mediana por turno **11.031 ns**; sem impacto material que
  justifique nova abstração/cache.

Não foi confirmado gargalo material nos caminhos medidos.

## Gates finais

- `cargo fmt --all -- --check`: exit 0.
- `cargo check --workspace`: exit 0.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0.
- `cargo test --workspace -j 1 --no-fail-fast`: 82 suítes, **821 passed**,
  0 failed, 1 ignored, 0 warnings.
- `.\refresh-slim.ps1 -Test`: `OK:`; release implantado.
- `target\release\slim.exe` e `C:\Users\User\bin\Slim.exe`: 12.982.272 bytes,
  SHA-256 `27A6DAC1F0393A9C322EE2084FAA081D1D27733FFEEA0C19D27C71DE45B2338B`.
- Smoke: `slim 0.1.0`; há um único `Slim*.exe` no diretório de PATH.

## Limitações reais

- Nenhum provider comercial foi chamado; latência, quota, cobrança e refresh
  contra serviços reais continuam não validados neste gate offline.
- O único teste ignorado exige host ConPTY físico; as matrizes sintéticas e o
  lifecycle de terminal passaram.
- O delta de working set de uma execução curta não prova ausência de leak; ele
  apenas não revelou crescimento incompatível com o histórico retido.
- O WIP externo foi preservado: snapshot final com 194 entradas, 98 tracked e
  96 untracked (inclui este relatório); nenhum
  reset/restore/checkout/clean/stash/pull/fetch foi usado.
