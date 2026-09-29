# Release do Slim

Este arquivo registra somente o deploy vigente e o procedimento reproduzível.
Deploys anteriores estão em [`history/2026-09.md`](history/2026-09.md).

## Deploy local vigente — governador, jobs de shell e contabilidade de uso (2026-09-29)

`.\refresh-slim.ps1 -Test` terminou com exit 0 e `OK: Slim slim 0.1.0`.
Suíte do workspace inteiro: 2.415 aprovados, 0 falhas, 41 ignorados, 64 suítes
(259 s de testes). Build release padrão: 427,29 s pelo wrapper, com um job Cargo.
Executável no PATH: `C:\Users\Thiago Emanuel\bin\Slim.exe`, 19.953.152 bytes.
SHA-256 do instalado e do `target/release/slim.exe` idênticos:
`D25411F18E09422A1B0475F9C01CF03A6314C089067364A81EF0CDB9E7BEDB89`.
Revisão do build: `a4d638691ca301ef56247616785d97d694e79db2-dirty`.
`Get-Command Slim` confirmou essa cópia; `--version` terminou com exit 0.

Segunda passada no `crates/slim-core/src/runtime/`, sobre `governor`, `loop_guard`,
`shell_jobs`, `economy`, `usage`, `mode`, `workspace`, `capability_bridge` e
`manual_retry`. Sem mudança de contrato público, exceto a contagem de
`tool_calls_reused` (abaixo).

- **Estrutura:** `governor.rs` virou `governor/{mod,compaction,evidence,observation,tests}.rs`;
  `UsageTotals::from_events` (370 linhas) virou um `LedgerBuilder` com métodos por família de
  evento; `observe_after`, `observe_evidence`, `compaction_snapshot`, `execute_managed_shell` e
  `start` foram decompostos. Funções acima de 100 linhas no módulo: 9 para 1 (a que resta é
  `initial_paths`, que contém a confinação de caminho e não foi tocada).
- **Tamanho:** o módulo cresceu de 6.259 para 6.731 linhas (+7,5%): produção de 3.469 para
  3.745 e testes de 2.790 para 2.986. Não houve redução de volume: helpers, tipos de fase e
  guardas das correções custam mais do que a duplicação removida.
- **Correções de comportamento**, cada uma com teste que falhava antes:
  - a validação que termina depois de uma mutação não certifica mais o estado novo como
    "verde" (a revisão gravada é a do início da chamada);
  - `shell_jobs`: `elapsed_ms` é fixado na conclusão do job, não na entrega; a entrega segue
    ordem numérica (`shell-2` antes de `shell-10`); um cancelamento já pedido impede o
    comando de nascer (o executor de shell só conferia o token depois de criar o processo);
  - no Windows, `write New.txt` e `write new.txt` de um arquivo ainda inexistente não entram
    mais juntos no mesmo cluster de mutações paralelas (chave de agendamento sem distinção
    de caixa; `path_identity` ficou intocado porque alguns chamadores usam o texto como caminho);
  - `tool_calls_reused` passa a contar `ToolEvidenceReused` com `post_compaction:false` (antes
    ficava sempre 0 no JSON e no texto do headless); o teste de contrato do fixture antigo
    continua 0 porque aquele evento era `post_compaction:true` (reaquisição, não reuso);
  - `normalize_output` só corta o sufixo ` | ms` quando há dígitos;
  - `LoopGuard` guarda um hash do erro, não o texto inteiro, e usa um único slot para a
    repetição imediata de shell/write/patch.
- **Neutro:** `hash_fields` deixou de existir em duas cópias; a cadeia `preparation_us`, sem
  leitor, foi removida; `CompactionSummary` acumula sobre `UsageBreakdown` em vez de um
  `UsageTotals` inteiro; `Runtime::can_ask(mode)` substitui três cálculos do gate de
  `ask_question`; `TempRoot` único de teste com limpeza automática; `Rig` de testes do governor.
- **Testes:** `slim-core --lib` de 393 para 415; `contracts` de 59 para 61.

Não alterado por decisão: `path_identity` (texto e chave são usados juntos), o `status` de job
concluído que repete a saída, limites de tempo em `shutdown`/`await` dos jobs, os limites sem
aviso do governador (256 dependências observadas, 4.096 fingerprints), a oscilação A→B→A que
conta como progresso, e a divergência entre as duas definições de "uso conhecido" (ledger contra
acumulador de compactação; nenhum provider atual as separa).

Não foram executados: console físico (ConPTY real) e sessão com provider real.

## Build e deploy local

```powershell
.\refresh-slim.ps1
.\refresh-slim.ps1 -Test
.\refresh-slim.ps1 -FastBuild
```

O script compila `slim-cli` em release, copia o executável para
`%USERPROFILE%\bin\Slim.exe` e executa o smoke de versão. O parâmetro `-Test`
roda antes `cargo test --workspace`.
`-FastBuild` aplica a variante local medida no [histórico](history/2026-09.md); execute sem a opção para
restaurar o perfil padrão no PATH.

Ambas as etapas usam prioridade BelowNormal, um job Cargo e relatorio
`--timings` em `target/cargo-timings/`; os testes usam duas threads do harness.
Os tempos totais aparecem no terminal. Para validar apenas os alvos afetados
antes de publicar, consulte o [fluxo de testes](../tests/README.md).
Se esses checks ja passaram, publique sem `-Test` para evitar repeticao.

## Pacote reproduzível

Com `target/release/slim.exe` já compilado:

```powershell
python3 release/build_release.py
python3 release/build_release.py
sha256sum -c release/SHA256SUMS.txt
7z l release/slim-0.1.0-windows-x64.zip
.\target\release\slim.exe --version
.\target\release\slim.exe --help
.\target\release\slim.exe --headless --unknown-option
```

As duas execuções do builder devem produzir o mesmo SHA-256 do ZIP. O pacote
deve conter somente `slim.exe`, com timestamp fixo, sem credenciais, sessões ou
artefatos locais. A opção desconhecida deve terminar com exit `30`.

## Limites

O pacote representa um checkpoint de integração parcial, não uma v1 completa.
Antes de distribuir, gere artefatos novos a partir do checkout validado e
registre hash, tamanho, data, comandos executados e limitações observadas.
