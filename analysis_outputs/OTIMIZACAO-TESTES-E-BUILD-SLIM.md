# Otimização dos testes e build do Slim

Data: 2026-09-04 (medições atravessam 2026-09-05 UTC). Workspace: `D:\Slim`.

## Resultado

**Verificado:** a mediana de `cargo test --workspace` já compilado caiu de **18,735 s para 10,183 s (45,7%)**. A recompilação incremental da TUI, incluindo linking dos seus testes, passou de **6,144 s para 2,716 s de mediana (55,8%)**, em duas amostras comparáveis por versão. São medições locais pequenas, com variabilidade, não promessa para qualquer máquina ou tarefa.

O menor conjunto implementado tem três partes:

- Fixtures de budget agora atendem a chamada final sem tools com HTTP 503 imediato, preservando a falha de finalização e o fallback.
- Dezessete arquivos de testes em memória da TUI compartilham um executável, mantendo os módulos, cenários e asserções.
- Dois testes que verificavam apenas operações da biblioteca padrão foram removidos.

Esta tarefa não alterou fontes de produção, manifests, lockfile, dependências, features, perfis Cargo, build scripts, configuração pessoal ou toolchain. Não houve chamada paga, commit, limpeza de target ou publicação externa. Alterações concorrentes de TUI apareceram depois das medições e são discriminadas na validação abaixo.

## Baseline e preservação

Foram lidos `AGENTS.md`, `RULES.md` e os cinco relatórios em `analysis_outputs/HARNESS-SLIM-ETAPA-{1..5}.md`. O checkout começou com **60 arquivos rastreados modificados e 13 entradas não rastreadas no status resumido**, incluindo as cinco etapas e OAuth/xAI. Esses trabalhos não são autoria desta otimização.

O status inicial e hashes dos arquivos em `crates`, `tests`, manifests, lockfile e script de deploy foram registrados em `%TEMP%\slim-test-build-20260905`. A comparação concluída após as medições identificou somente quatro conteúdos preexistentes alterados: `agent_loop.rs`, `provider_cli.rs`, `headless_resume.rs` e `fault_injection.rs`. Naquele snapshot, os outros 16 arquivos movidos conservaram exatamente seus bytes, incluindo o WIP de `layout_golden.rs`; fontes de produção e configurações Cargo conservaram seus hashes de entrada. A conferência posterior detectou trabalho concorrente em mais três arquivos, preservado e descrito abaixo, sem atribuí-lo à otimização.

A medição de invalidação incremental atualizou apenas o timestamp de `crates/slim-tui/src/lib.rs`, sem alterar seu conteúdo. O target existente foi preservado integralmente.

## Gargalos e mudanças

### 1. Fixtures encerravam antes da finalização de budget

**Verificado por execução e leitura dos servidores:** as fixtures enviavam as chamadas de tools e encerravam o listener. O runtime corretamente tentava a chamada final sem tools, mas encontrava conexão recusada; no Windows observado, esse caminho custava aproximadamente dois segundos por ocorrência. Não era trabalho de modelo nem compilação. Não se atribui todo esse intervalo a um timer interno específico.

A amostragem diagnóstica serial dos executáveis existentes localizou:

| Fixture | Tempo anterior aproximado | Comportamento preservado |
|---|---:|---|
| `bounded_stops_expose_stable_status_and_nonzero_codes` | 4,048 s, dois cenários | Códigos e JSONL estáveis de turn_limit e tool_limit |
| `resume_tool_call_is_blocked_before_side_effect_and_records_failed_terminal` | 2,027 s | Write impedido, ausência de arquivo e terminal durável Failed |
| `tool_limit_blocks_excess_mutating_calls_before_execution` | 2,084 s | Só a primeira mutação cabe; a segunda não executa |
| `one_provider_batch_executes_tools_serially_in_source_order` | 2,028 s | Ordem e efeitos do lote serial |
| `mixed_batch_parallelizes_contiguous_reads_without_crossing_write_barrier` | 2,041 s | Reads concorrentes, resultados ordenados e barreira de escrita |
| `validation_shell_joins_the_parallel_read_segment` | 2,230 s | Shell allowlisted inicia no mesmo segmento dos reads |
| `read_exhaustion_stops_with_tool_limit` | 2,045 s | Orçamento de leitura e quantidade executada |
| `max_total_tool_calls_stops_across_multiple_turns_with_tool_limit` | 2,046 s | Budget total entre turnos e histórico dos resultados |
| `mixed_batch_budget_preserves_the_execution_prefix` | 2,021 s | Prefixo do lote; nenhuma escrita após operação suprimida |

O helper `tests/support/budget_finalization.rs` é compilado como módulo nos três arquivos afetados. Ele atende somente essa fase: aguarda o request localhost, lê o corpo declarado, exige ausência de tools e o aviso de budget e devolve HTTP 503 sem espera artificial. Tem limites de leitura, tamanho e escrita. Reutiliza std e serde_json já presentes; não introduz servidor genérico ou framework.

Nenhum desses nove testes foi removido; nenhum timeout do produto foi reduzido ou aumentado. A falha continua real, agora HTTP explícito, e os resultados de budget/retomada continuam sujeitos às mesmas asserções. A cobertura específica de timeout e cancelamento HTTP permanece em `provider_http` e `runtime_abort`. A finalização bem-sucedida e o cancelamento durante finalização continuam cobertos pelos casos existentes em `agent_loop`.

O resultado de conexão recusada não é tratado como equivalente a todos os erros HTTP. O que estas fixtures precisam proteger é a manutenção do resultado de budget/efeitos quando a resposta final falha; não o tempo do stack TCP do Windows.

### 2. Compilação/linking repetidos na TUI

Antes: 19 executáveis de integração, mais o executável de unit tests e doc-tests. Vários arquivos com poucos casos repetiam frontend/codegen/linking e inicialização de processo. Os testes de renderização/estado examinados usam memória local, sem alteração global de ambiente/cwd, servidores ou arquivos de fixture.

Agora `crates/slim-tui/tests/integration/main.rs` declara 17 módulos. A descoberta automática normal do Cargo foi mantida; não há lista paralela de targets no manifesto nem `autotests=false`.

| Módulos preservados | Proteção mantida |
|---|---|
| `m0_frames`, `m2_integration`, `m3_golden`, `fault_injection` | Projeção, estado, eventos causais, coalescimento, cancelamento e falhas |
| `layout_golden`, `poc2_golden`, `golden_matrix`, `welcome_golden` | Layout, tamanhos, métricas tok/s e snapshots de terminal |
| `scroll_golden`, `markdown_golden`, `motion_activity_golden` | Scroll, renderização de texto e atividade |
| `multi_tool_golden`, `tool_details_golden`, `thinking_expansion_golden` | Identidade/lifecycle das tools, detalhes e expansão de reasoning |
| `inspectors_golden`, `model_overlay_golden`, `interaction_roundtrip` | Overlays, modelos, perguntas e comandos do reducer |

Os arquivos passaram de `tests/<nome>.rs` para `tests/integration/<nome>.rs`. O grupo tinha 188 testes e agora executa 186, com as duas remoções justificadas abaixo. `properties.rs` permanece separado para seus casos proptest; `pty_windows.rs` permanece separado porque contém o gate de latência de 50 mil inserções. Esse último nome histórico não representa o ConPTY físico da CLI, que continua separado e ignored.

A TUI passa de **20 para 4 alvos executáveis de teste** a compilar/linkar quando sua biblioteca muda; as suítes incluindo doc-tests passam de 21 para 5. No workspace: **84 para 68 executáveis**, ou **88 para 72 suítes incluindo os quatro doc-tests**.

A [documentação primária do Cargo](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests) descreve esse custo por executável e o agrupamento por módulos. O ganho aqui foi medido no checkout; não inferido apenas da documentação.

Comandos focados atuais:

```powershell
cargo test -p slim-tui --test integration layout_golden::
cargo test -p slim-tui --test integration fault_injection::
cargo test -p slim-tui --test properties
cargo test -p slim-tui --test pty_windows
```

Uma edição isolada em qualquer módulo recompila o executável compartilhado. Esse custo não foi comparado ao antigo executável pequeno; o ganho demonstrado é para alteração da biblioteca TUI e execução integral. Não se afirma aceleração de todo comando seletivo.

### 3. Remoções justificadas antes da edição

| Teste removido de fault_injection | O que realmente verificava | Por que a remoção é suficiente |
|---|---|---|
| `panicking_block_renderer_degrades_to_fallback_row` | O próprio teste chamava panic dentro de catch_unwind, depois verificava renderização normal | Nunca passava pelo `runtime::safe_block_lines` do produto nem verificava sua linha de fallback. Não protegia a recuperação anunciada pelo nome. Os testes reais de renderização permanecem; injeção de panic nessa fronteira continua uma lacuna, já existente antes. |
| `command_channel_disconnect_surfaces_as_error` | Criava std::sync::mpsc, descartava receiver e conferia send com erro | Não chamava canal/adaptação/tratamento de erro do Slim. Testava contrato da biblioteca padrão, sem cobertura de comportamento do produto a substituir. |

Foi removido também o import mpsc que ficou sem uso; o comentário do módulo agora descreve somente os casos que realmente exercita. Não se atribui ganho mensurável de tempo a essas duas remoções individualmente: seu benefício é eliminar manutenção e falsa evidência. Não se usou redução de contagem como métrica de velocidade.

## Medições comparáveis

Ambiente verificado: Windows x86_64 MSVC, rustc **1.98.0 (88d9e12ae 2026-08-18)** e Cargo **1.98.0 (797e8a9bc 2026-08-05)**, executáveis do toolchain ativo Scoop. RUSTC/RUSTDOC/CARGO foram apontados explicitamente somente no processo dos comandos; CARGO_NET_OFFLINE=true. Jobs/perfis/threads de teste permaneceram nos defaults existentes.

Stopwatch de PowerShell mediu wall incluindo Cargo e redirecionamento para log. Nenhuma medição comparativa válida rodou junto com outro build ou diagnóstico de testes iniciado por esta tarefa. A execução diagnóstica serial serviu para localizar os casos; não foi comparada com o gate paralelo.

| Medição | Antes | Depois | Leitura |
|---|---|---|---|
| Workspace já compilado: `cargo test --workspace`, três amostras | 18,735 / 18,768 / 18,614 s | 19,248 / 10,183 / 9,891 s | Medianas 18,735 → 10,183 s, -45,7%. A primeira amostra posterior foi lenta e não foi omitida. |
| Soma dos tempos das suítes nessas execuções | 16,29 / 16,33 / 16,28 s | 8,98 / 8,21 / 7,94 s | Medianas 16,29 → 8,21 s, -49,6%; inclui arredondamento de libtest. Não inclui overhead Cargo/processos/doc compilation. |
| Recompilação TUI: tocar timestamp de src/lib.rs; `cargo test -p slim-tui --no-run --timings` | 6,934 / 5,353 s | 2,774 / 2,658 s | Medianas 6,144 → 2,716 s, -55,8%. Dependências e código incremental já disponíveis, mesmos perfis/comando. |
| Freshness check inicial: `cargo test --workspace --no-run --timings` | 0,457 s | Não tratado como comparação de ganho | O target inicial já estava compilado. |
| Preparação após mudanças: `cargo test --workspace --no-run --timings` | — | 12,754 s | Recompila alterações/variantes; excluído do ganho de execução. |

Na segunda execução completa comparável, os tempos internos passaram de: provider_cli **4,04 → 0,09 s**; headless_resume **2,04 → 0,07 s**; agent_loop **2,21 → 0,32 s**. Os 17 executáveis de memória da TUI somavam aproximadamente **0,14 s** de libtest e agora seu grupo leva **0,05 s**, além de eliminar 16 inicializações de processo.

Preparações/experimentos excluídos da comparação incremental: a primeira chamada isolada TUI levou 16,704 s e recompilou slim-core por diferenças na unificação de features do comando de pacote versus workspace; outra levou 5,431 s enquanto o diagnóstico serial ainda estava ativo. A primeira compilação do novo grupo levou 2,268 s, mas não invalidou a biblioteca nas mesmas condições; também não foi usada como ganho.

**Limites por fase:**

- **Compilação inicial sem cache:** não medida; não necessária para decidir estas alterações. Nenhum ganho de clean build é alegado e nenhum target foi apagado.
- **Recompilação incremental de produção:** sem mudança de fonte ou perfil; não se reivindica ganho no build do aplicativo.
- **Compilação dos testes + linking:** ganho TUI medido acima, com redução comprovada de alvos.
- **Linking isolado:** não foi cronometrado separadamente do rustc. O HTML de `--timings` desta execução tem `sections: null`; os tempos dos alvos abrangem compilação/linking concorrentes. Não foram somados como CPU nem publicados como tempo puro do linker. Veja o [limite da instrumentação Cargo](https://doc.rust-lang.org/cargo/reference/timings.html).
- **Execução:** mediana e amostras completas acima. A causa da amostra posterior de 19,248 s não foi isolada; Cargo informou build atualizado em 0,33 s e as suítes somaram 8,98 s. Não se apresenta sua diferença como compilação ou cache economizado.

## Preservação das cinco etapas e validação

A comparação dos nomes executados, normalizando somente os novos prefixos dos 17 módulos, encontrou exatamente as duas remoções declaradas e nenhum outro caso perdido. A suíte atual conserva descarte de future/estado, cancelamento, budgets e prefixos, shell falho, resultados completos, histórico, autoridade/pedido recente, compactação, reasoning opaco, término nativo, usage em falhas e configuração dos providers. Nenhuma asserção desses contratos foi enfraquecida.

- TUI focada: **329 passed**, 0 failed, cinco suítes incluindo doc-tests.
- Fixtures focadas: `cargo test -p slim-core -p slim-cli --test agent_loop --test provider_cli --test headless_resume` → **67 passed**, 0 failed (49 + 10 + 8).
- Três execuções completas posteriores: **1105 passed / 0 failed / 1 ignored**, 72 suítes, sem skip.
- `cargo clippy --workspace --all-targets -- -D warnings` → EXIT=0.
- Rustfmt nos três testes editados, helper, main e fault_injection → EXIT=0.
- `cargo fmt --all -- --check` → EXIT=1 por drift preexistente, incluindo OAuth/TUI/LSP. Na conferência anterior ao trabalho concorrente, os hashes de fontes de produção eram iguais à entrada; não foi aplicado fmt global.
- `git -c core.safecrlf=false diff --check` → EXIT=0.

Gate da otimização, anterior às alterações concorrentes abaixo: `.\refresh-slim.ps1 -Test` → **EXIT=0**, `cargo test --workspace` sem filtros: **1105 passed / 0 failed / 1 ignored / 72 suítes / 0 compiler warnings**. O ignored continua sendo ConPTY físico. Build release levou 1m36s; esse tempo é evidência do deploy, sem comparação de ganho de release.

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 21:24:01)
```

Após concluir esse script, target e PATH foram conferidos: **15.011.840 bytes**, SHA-256 idêntico **`E6036EA7D78DB06C90BA709E7C78058254D6F997E7161AEFD83E95228A61E909`**. `Slim.exe --version` → `slim 0.1.0`, EXIT=0; `--headless --fake --prompt 'test build optimization offline smoke'` → `success`, EXIT=0. O tamanho era igual ao observado antes do rebuild; o hash mudou. Não se afirma identidade binária com a etapa 5 nem ganho de desempenho de produção.

### Alterações concorrentes após as medições

A última conferência encontrou edições de outro trabalho em `crates/slim-tui/src/theme.rs` (21:26:42), `src/runtime.rs` e `tests/integration/golden_matrix.rs` (21:27:40), posteriores ao primeiro deploy. Não foram produzidas nem revertidas por esta tarefa. A fonte de produção permanecera intacta durante as medições comparativas; portanto, seus ganhos não são atribuídos à mudança visual concorrente.

O runtime recebeu mais uma edição concorrente às 21:29:40. A primeira tentativa de novo gate abortou ao compilar os testes: Windows recusou remover `target/debug/slim.exe` com `Acesso negado (os error 5)`. O script não fez deploy após essa falha. Não houve encerramento de processos, exclusão de artefatos ou correção especulativa. Sem processo Slim restante na conferência seguinte, Clippy e `refresh-slim.ps1 -Test` foram repetidos para verificar/deployar o checkout combinado. As mudanças de fonte e o bloqueio transitório justificam repetir o gate, sem refazer as medições ou atribuir o trabalho concorrente à otimização.

O gate combinado executou **1106 passed / 0 failed / 1 ignored / 72 suítes / 0 compiler warnings**. A diferença em relação às medições é exatamente a nova regressão concorrente `runtime::tests::long_composer_model_keeps_identity_effort_and_mode_with_cell_safe_elision`. Contabilidade: baseline 1107 − dois casos std + um caso concorrente = 1106. Clippy combinado: EXIT=0. Os tempos comparativos continuam sendo os do snapshot de otimização, com 1105 passes; não foram transplantados para a versão visual concorrente.

**Deploy final combinado concluído, EXIT=0:**

```text
OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 21:32:54)
```

Target/PATH: **15.014.912 bytes**, SHA-256 **`21F77F9D7C828F6D2A5E69731DAF465F1261961A5873CC842AF265F210B2E562`**, idênticos após o script. Versão e smoke fake reexecutados no binário final: `slim 0.1.0` / `success`, ambos EXIT=0. Comparação dos hashes de todos os arquivos existentes em crates/tests antes e depois deste último gate: **zero drift**. O release levou 1m21s; não é amostra comparativa de desempenho. Logs finais adicionais: `clippy-final-retry.log`, `refresh-final-retry.log` e `hashes-before-final-retry.json`.

## Oportunidades descartadas e limitações restantes

- **Unificar core/CLI inteiros:** descartado. Há mutação de ambiente, locks locais por executável, paths por PID e fixtures de rede/processos. A união indiscriminada exigiria novo isolamento e poderia invalidar os ganhos de determinismo. A TUI escolhida evita essa interferência.
- **Alterar release LTO, codegen-units, opt-level ou debug:** não implementado. O custo observado estava nos testes; mudar otimização/diagnóstico/tamanho do produto não seria uma troca justificada.
- **Novos caches, linker, toolchain ou dependências:** não implementados. A árvore atual já usa reqwest sem default-features e ratatui sem defaults. `cargo tree --duplicates` mostrou versões transitivas exigidas por dependentes diferentes (bitflags/getrandom/windows etc.); duplicação de nome sozinha não justifica migração. Não há build.rs próprio nas quatro crates.
- **OAuth xAI (~2 s):** polling de autorização real com localhost e intervalo contratual; preservado. Virtualizar o relógio com I/O real exigiria verificar deadlines e habilitar suporte específico. Não é sleep descartável sem preservar esse contrato.
- **Processos/limites grandes:** tool_contracts, cancelamento da árvore de processos, LSP e limite de 64 MiB continuam com custo real. Não foram substituídos por mocks que deixariam de proteger esses comportamentos.
- **Outras pequenas duplicações/sleeps:** não foram perseguidas após os ganhos principais; helpers de smoke e campos default não foram removidos só pela aparência trivial. Não houve nova infraestrutura de benchmark.
- **Sem prova live:** providers comerciais, console físico/ConPTY, autenticação real, performance comercial e ZIP de distribuição não foram validados ou regenerados.

Logs temporários essenciais: `baseline-hot-{1,2,3}.log`, `after-hot-{1,2,3}.log`, `tui-before-rebuild-{2,4}.log`, `tui-after-rebuild-{1,2}.log`, HTMLs correspondentes, `fixtures-focused.log`, `clippy-final.log`, `fmt-global.log` e `refresh-final.log`, no diretório de medição citado. As conclusões e números necessários estão reproduzidos neste relatório.

## ✅ Verificação de Entrega — RULES.md §4

- [x] Comandos e resultados executados nesta sessão registrados acima.
- [x] Contagens atuais obtidas da saída integral, sem reutilizar as cinco etapas.
- [x] Arquivos/símbolos citados lidos; sem linhas presumidas.
- [x] `cargo test --workspace` final combinado verde: 1106 passed, 0 failed, 1 ignored, 0 compiler warnings; autoria do teste concorrente separada.
- [x] `refresh-slim.ps1 -Test` concluído com OK e identidade target/PATH.
- [x] Índices/status atualizados; contagens correntes antigas substituídas, mantendo os gates históricos das etapas anteriores.
- [x] Incertezas e limites declarados: cold build/linking puro não medidos, amostra lenta preservada, fmt preexistente, live/ConPTY/ZIP não aplicáveis à entrega local.
