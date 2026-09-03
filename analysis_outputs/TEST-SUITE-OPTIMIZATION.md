# Auditoria e otimização da suíte de testes

Data: 2026-08-27  
Escopo: testes do workspace `D:\Slim`; sem alteração de comportamento de produto.

## Resultado

Medição quente do comando `cargo test --workspace -j1 --no-fail-fast`:

| Estado | Tempo de parede | Suítes | Passed | Failed | Ignored |
|---|---:|---:|---:|---:|---:|
| Antes | 21,878 s | 82 | 865 | 0 | 1 |
| Depois (mediana de 3) | 10,736 s | 82 | 863 | 0 | 1 |

Amostras finais: 10,963 s, 10,736 s e 10,568 s. A redução observada foi de
**50,9%**. O total caiu em dois casos porque ambos eram estritamente
redundantes, não por redução de cobertura contratual.

## Testes descartados

Em `crates/slim-core/tests/tool_contracts.rs`:

- `shell_drains_large_stdout_and_stderr_while_child_runs`: subsumido por
  `shell_caps_raw_streams_while_draining_the_remainder`, que também cobre stdout
  e stderr simultâneos, drenagem integral e contabilização de descarte sob cap;
- `shell_stream_above_cap_is_truncated_with_marker`: subsumido por
  `shell_marker_counts_raw_and_context_discarded_bytes`, que verifica o mesmo
  marcador e ainda confere exatamente os bytes descartados nas duas camadas.

## Otimizações preservando os contratos

- `properties.rs`: reutilização do `WrapCache`, geração proporcional ao contrato
  de overflow e substituição de 256 sequências que sempre terminavam em `End`
  por uma matriz determinística de todos os seis `ScrollIntent`;
- `skill_invocation.rs`: cancelamento validado pela ausência real do PID filho,
  sem esperar 2 s após cada caso;
- `provider_http.rs`, `tui_bridge.rs` e `runtime_abort.rs`: sleeps de servidor
  trocados por canais de liberação causal; o consumidor continua observando o
  evento exigido antes de cancelar/liberar;
- `cli_contract.rs`: endpoint morto com timeout substituído por fixture SSE
  localhost; a fixture consome headers e corpo HTTP completos antes de responder;
- `provider_cli.rs`: a ausência de contato com o provider é provada por um
  `accept` imediato após o erro síncrono, sem polling fixo de 500 ms.

## Tentativas rejeitadas

- agrupar a escrita de 8 MiB do contrato de cap em blocos maiores não melhorou
  o teste (aprox. 752 ms contra 748 ms) e foi revertido;
- cancelar imediatamente dois testes de runtime produziu RED legítimo
  (`input_tokens` observado como 0 em vez de 7); a janela causal de 50 ms foi
  restaurada e só a espera artificial do servidor foi removida;
- `--report-time` do libtest requer nightly neste toolchain; a comparação usa
  `Stopwatch` externo e tempos internos por suíte, sem flags instáveis.

## Custos mantidos por necessidade

Foram preservados os testes de limite exato de header de 64 MiB, morte de árvore
de processos, timeout/cancelamento de shell, cancelamento pelo bridge TUI e
integração PowerShell de Skill. Eles são relativamente caros, mas exercitam
fronteiras que testes menores não substituem.

## Verificação final

- dez repetições de `cli_contract`: 10/10 verdes após robustecer a fixture TCP;
- três execuções completas medidas: 863 passed / 0 failed / 1 ignored em todas;
- `cargo fmt --all -- --check`, `cargo check --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings` e
  `git diff --check`: exit 0;
- `refresh-slim.ps1 -Test`: exit 0 e `OK:`;
- target/PATH: 13.830.144 bytes, SHA-256
  `DBFE58EE27FCE7D2F2001D88A7A4F0A6591F58A9AE911D0070A87B2FBD0008C3`;
  `slim --version` = `slim 0.1.0`.

Limite: os tempos de parede variam com o agendamento e antivírus do Windows. A
mediana de três execuções quentes reduz esse ruído; não é benchmark de produto.
