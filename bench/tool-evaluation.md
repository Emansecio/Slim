# Avaliação do candidato integrado — preparada em 2026-09-07

Esta preparação não executa modelos. Avalia o estado atual; não há baseline
anterior com proveniência suficiente para atribuir diferenças a estas mudanças.
Os executáveis antigos de `bench/economy-next` não foram adotados como baseline.
Não reconstruir um anterior removendo alterações do checkout.

## Consolidação executada

Checkout com alterações sem commit sobre HEAD
`972b46e4e445de05b4021554b277710f3cfe9243`. O HEAD não identifica estes fontes.
Manifestos SHA-256 por arquivo em `../target/consolidation/` cobrem 253 entradas:
Cargo.toml/Cargo.lock, crates, testes e configuração local de build quando presente;
incluem arquivos não rastreados e marcadores de ausentes, excluindo sessões `.slim`.
Documentação e resultados de benchmarks não integram esse identificador de fontes.

| Estado | SHA-256 do manifesto |
|---|---|
| Inicial, antes de editar (`sources-before.sha256`) | `26EA8F07CEDA3ECCD22030FFBE7C02882E0798CBB801A22D82891D7D7ED99AF1` |
| Suíte integrada (`sources-validated.sha256`) | `B0451FC85ACA39F2E174494820CE1C6AA49DF0E014CCD22EBA160D9F33CF67AD` |
| Candidato (`sources-candidate.sha256`) | `F86D6713068A69786B067642C859ED362D369A5CD9F81751B9180F3E87F486F7` |

O segundo estado inclui somente a correção da fixture `process_tree.rs`.
O terceiro acrescenta somente `0..=MAX_SEARCH_SNAPSHOTS` no teste de busca.
Os manifestos antes/durante/depois da suíte coincidem; os manifestos antes/durante/
depois do release também coincidem. Não foi detectada alteração concorrente nesses
arquivos. Isso não é um snapshot transacional nem identifica conteúdo externo ao
manifesto; documentos/WIP externos foram preservados.

Integrações lidas e cobertas pela suíte: TUI (`grouped_thinking_lines`, recorte e
cache de alturas); preparação de requests (`prepare_messages_request_with_tools_checked`,
afinidade pelo prefixo estável); runner Windows (`wait_for_process_progress`, Job
Object e drenagem); cache de evidências (`lookup_cached_evidence`, referência Arc liberando
o mutex antes de revalidar/copiar); search (schema → parser → SearchPageOptions →
snapshot/formatador → resultado nativo). Nenhuma nova otimização de produto.

Clippy reproduziu `zombie_processes` em `process_tree.rs:203`, exit 101. O filho
espera a saída do pai antes de escrever: acrescentar wait quebraria esse contrato.
A fixture agora transfere Child para OwnedHandle e o fecha explicitamente; o job
do runner continua responsável pela árvore. Nenhuma nova supressão. A regressão
`runner_drains_output_written_after_the_direct_child_exits` passou na suíte.
O diagnóstico já aparecera na rodada anterior, mas o arquivo é não rastreado:
isso não demonstra preexistência no estado original do Git.
O Clippy seguinte revelou `range_plus_one` no teste `search.rs:1171`, exit 101;
o intervalo inclusivo preserva o mesmo número de inserções e as mesmas assertions.

Toolchain: rustc/rustdoc 1.98.0 (`88d9e12ae`, 2026-08-18), Cargo 1.98.0
(`797e8a9bc`), host/target x86_64-pc-windows-msvc. RUSTC/RUSTDOC foram fixados
somente nos processos de build ao diretório
`C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin`.
Cargo foi resolvido por `C:\Users\User\scoop\apps\rustup-msvc\current\.cargo\bin\cargo.exe`.
`CARGO_BUILD_JOBS=1`; wrapper sccache configurado globalmente foi desativado apenas
nestes comandos por `--config 'build.rustc-wrapper=""'`. Nenhuma configuração global
foi editada. RUSTFLAGS/RUSTDOCFLAGS/CARGO_TARGET_DIR não definidos; sem configuração
`.cargo/config.toml` no projeto. Dependências fixadas por `--locked`, features padrão.

| Check efetivamente executado (Cargo com o override acima) | Resultado |
|---|---|
| `cargo test --workspace --locked --no-fail-fast` | exit 0; 1.260 passaram, zero falhas, 19 ignorados; inclui quatro Doc-tests com zero casos. Estado B045… |
| `cargo test -p slim-core --locked --lib tools::search::tests::context_edges_overlap_unicode_and_snapshot_storage -- --exact` | exit 0; 1 passou no estado F86D…; suíte completa não repetida |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` final | exit 0, estado F86D…; verifica compilação dos alvos do workspace |
| `cargo fmt --all -- --check` final | exit 1; diferenças em 12 arquivos do estado inicial, preservadas |
| `rustfmt --edition 2021 --check crates/slim-core/tests/process_tree.rs crates/slim-core/src/tools/search.rs` | exit 0 |
| `git diff --check` | exit 0 |
| `cargo build --release -p slim-cli --locked` | exit 0; mesmo comando de build do fluxo existente, sem executar a etapa de instalação |
| Candidato e instalado com `--version` | ambos `slim 0.1.0`, exit 0; não abre TUI |

Os 19 ignorados são: 1 dependente de ConPTY real, 9 fixtures de subprocesso e
9 medições manuais de desempenho. As fixtures são chamadas pelos testes que as
usam, mas sua marcação individual continua ignored, com a justificativa original.
Não executado benchmark manual, provider comercial ou validação visual nesta rodada.

Pendência de formatação: slim-cli `src/config.rs`, `src/oauth/{mod,store,xai}.rs`;
slim-core `src/session/capabilities.rs`, `tests/{session_capabilities,tool_contracts}.rs`;
slim-lsp `src/pool.rs`; slim-tui `src/{inspector,reducer}.rs` e
`tests/integration/model_overlay_golden.rs`; `tests/e2e_offline.rs` na raiz.
Os detalhes estão em `../target/consolidation/fmt-final.log`. Não foi aplicada
formatação ampla sobre alterações acumuladas fora do ajuste localizado.
Outros logs: `workspace-test.log`, `search-regression.log`, `clippy-before.log`,
`clippy-after.log`, `clippy-final.log`, `release-build.log` e `release-build.json`.

Candidato: `D:\Slim\target\release\slim.exe`, 15.392.768 bytes;
SHA-256 `9FABC2BD008FA6B41F56C37E4166B8A09883DB446F00965C7634659FB8CB6C65`.
Build de 2026-09-08 02:31:02.480–02:34:02.701 UTC (07/09 à noite em São Paulo),
perfil release existente: opt-level 3, thin LTO, codegen-units 1, strip true.
Fontes correspondem ao manifesto F86D… completo acima.

Instalado/PATH: `C:\Users\User\bin\Slim.exe`, 15.376.896 bytes, modificado em
2026-09-07 07:46:55 UTC;
SHA-256 `DDE9BE3824FC13A0C99C5EF75929EAC376F96971854D1F46276E8EB64E89709E`.
Hash igual antes e depois. Não foi instalado/substituído; sua versão textual não
estabelece equivalência dos fontes e não lhe atribui os resultados do candidato.

Checklist RULES §4: evidência e checks acima; documentação desta avaliação criada;
deploy, smoke de instalação e TUI física não aplicáveis ao escopo. Nenhum commit,
push, reset, clean ou chamada a provider real. Sem nova medição comparativa de
desempenho: resultados anteriores continuam históricos. Falta observar com modelo
se contexto curto será escolhido corretamente, se haverá menos idas e voltas,
quando leituras amplas/recuperação serão necessárias e qual será o tempo total.

## Materiais e correção

As bases descartáveis estão em `D:\Slim\target\consolidation\evaluation\initial`.
O manifesto `..\initial.sha256` identifica 227 arquivos, 3.990.372 bytes;
SHA-256 `53d3f945e0fc70b6f5be9dd4fcb3b27bbee6876fb577e7b7c5cd0835f6acdbf0`.
São entradas fixas, não resultados de execução do candidato. Cada pasta contém
`PROMPT.txt`, `SPEC.md` e `check.py`. A sintaxe dos três validadores foi conferida.
Os três validadores foram executados offline sobre as bases iniciais e retornaram
exit 1, como esperado: paginação incorreta, resposta ainda ausente e configuração
ainda não migrada. Os hashes das entradas permaneceram iguais. Isso valida o
estado inicial, não é execução das tarefas com modelo nem prova completa do oracle.

| Pasta | Objetivo | Critério independente |
|---|---|---|
| `local_edit` | Corrigir paginação em `pager.cjs`; preservar o consumidor `listing.cjs`. Contexto curto pode ser suficiente. | `python check.py` deve passar: páginas a partir de 1, validação, cópia rasa, imutabilidade, última página e consumidor. Somente `pager.cjs` pode mudar. |
| `broad_investigation` | Investigar o snapshot Rust: busca literal, limites e TTL, com evidências de implementação e teste; explicar reutilização na paginação. | `answer.json` com 200/500 hits, 32 padrões, TTL 120 s e `literal`; `python check.py` confere fatos e linhas. Revisão humana deve confirmar que cada trecho realmente sustenta seu fato e que a explicação cobre a condição de validade do snapshot. Preservar todos os fontes. |
| `multi_file` | Migrar configurações de desenvolvimento e produção para schema 2. Há capacidades de lote disponíveis; sua utilização é decisão do agente. | `python check.py`: retry_policy, conversão de segundos, zero/fração, Unicode, ordem e metadados preservados. `config/current.json` permanece byte a byte. Só development/production podem mudar. |

Origem: `local_edit` extrai somente a definição literal `js_pagination` de
`bench/luna-live/daily.py`, sem importar ou executar seu runner. As outras duas
bases são cópias de `bench/economy-next/frozen/source_audit` e `config_migration`.
O snapshot investigado é material histórico da tarefa, não o fonte deste build.
Se for necessário recriar as bases, repetir essas cópias e a extração literal;
validar o manifesto antes de chamar qualquer modelo. Não executar `daily.py`
ou `scenarios.py` para preparar os arquivos: contêm caminhos de execução live.

Nenhum prompt determina search→read→patch ou comandos específicos de inspeção.
A investigação contém arquivos extensos: se o agente gerar resultado truncado,
observar se usa os artefatos/recuperação existentes. Se não houver truncamento,
registrar "recuperação não exercitada"; não induzir chamadas para obter uma métrica.
Não trocar por uma fixture artificialmente maior após observar o resultado.

## Execução futura, somente após autorização para providers

Para cada tentativa, copiar a pasta inicial para um diretório novo em `%TEMP%`.
Guardar resultados fora do workspace da tarefa. Exemplo PowerShell, a executar
em processo sem janela; substituir os três identificadores pelos mesmos valores
efetivos já autorizados, sem editar configuração global:

```powershell
$case = 'local_edit' # repetir separadamente para as outras duas pastas
$id = [guid]::NewGuid().ToString('N')
$work = Join-Path $env:TEMP "slim-eval-$case-$id"
$result = Join-Path $env:TEMP "slim-eval-result-$case-$id"
New-Item -ItemType Directory -Path $result | Out-Null
Copy-Item -LiteralPath "D:\Slim\target\consolidation\evaluation\initial\$case" -Destination $work -Recurse
$prompt = Get-Content -LiteralPath (Join-Path $work 'PROMPT.txt') -Raw
$candidate = 'D:\Slim\target\release\slim.exe'
$argsSlim = @('--headless', '--jsonl', '--provider', '<provider autorizado>',
  '--model', '<modelo autorizado>', '--effort', '<esforço autorizado>',
  '--session', (Join-Path $result 'session.jsonl'), '--prompt', $prompt)
Push-Location $work
try {
  $timer = [Diagnostics.Stopwatch]::StartNew()
  & $candidate @argsSlim 1> (Join-Path $result 'result.jsonl') 2> (Join-Path $result 'stderr.txt')
  $runExit = $LASTEXITCODE
  $timer.Stop()
  @{ exit_code=$runExit; elapsed_ms=$timer.Elapsed.TotalMilliseconds } |
    ConvertTo-Json | Set-Content (Join-Path $result 'wall.json')
  python check.py 1> (Join-Path $result 'check.txt') 2>&1
  $checkExit = $LASTEXITCODE
  "CHECK_EXIT=$checkExit" | Add-Content (Join-Path $result 'check.txt')
} finally { Pop-Location }
```

Antes: verificar hash do candidato contra a entrega, hash de cada arquivo copiado,
versões Python/Node, configuração efetiva, permissões, limites, modelo/esforço e
eventual modo fast. Manter tudo constante entre tentativas. Não copiar `.slim`,
credenciais, histórico ou arquivos gerados de uma execução para a seguinte.
O script não ajusta permissões. Registrar bloqueio de aprovação/entrada como tal.
Após: conferir arquivos permitidos e hashes dos preservados, além do validador.
Não apagar automaticamente workspaces ou logs. O tempo do validador independente
fica fora do tempo do agente; a validação que ele próprio solicitar fica dentro.

## Registro permitido e limitações

Usar o JSON final e a sessão durável v2. O journal atual persiste mensagens visíveis,
chamadas e resultados, descartando estado opaco de continuação; `record_event`
seleciona eventos de ferramentas. Não habilitar trace de transporte nem capturar
`ReasoningDelta`, `chat_reasoning`, `responses_reasoning` ou conteúdo privado.
Na análise, selecionar somente entradas de chamadas/resultados e métricas numéricas.
Não usar raciocínio privado para justificar repetição.

| Observação por execução | Fonte e interpretação |
|---|---|
| Correta/falha/inconclusiva | Exit real, `stop`, validador e diff/hashes. Exit 0 sozinho não prova correção. |
| Tempo total | Stopwatch externo; inclui inicialização e esperas. |
| Interações | `usage.provider_turns` e requests de `request_kind=provider_turn`; separar retry, compactação e batches de ferramentas. |
| Chamadas por ferramenta | Entradas assistant com `tool_calls`: id, nome, argumentos; associar entry tool por `tool_call_id`. Separar solicitadas, executadas, reutilizadas e suprimidas pelos totais disponíveis. |
| Repetições | Comparar nome/argumentos normalizados e revisão dos arquivos. Justificar apenas por resultado anterior, página/cursor, edição intermediária, truncamento, erro ou validação; caso contrário, "sem justificativa observável". |
| Resultados | Bytes UTF-8 do conteúdo tool persistido; registrar referência e tamanho de artefato quando presente. Distinguir excerpt, resultado retido e bytes de resultado reinseridos em requests (`tool_result_bytes`, contados novamente a cada envio). |
| Trabalho/tempo interno | `usage.tool_latency_ms` e requests; `provider_latency_ms`, TTFB e primeiro evento semântico separados. Latência do provider inclui transporte/processamento local e remoto, não é tempo puro de inferência. |
| Processos e operações internas | Os receipts medem `bytes_read`, `preparation_us`, `execution_us`, `finalization_us`, mas não são exportados pela sessão/JSON deste CLI. Registrar indisponível. Contar shell como shell, não como número de processos; descendentes, scans, syscalls, CPU e RSS também indisponíveis neste protocolo. |

`ToolFinished.duration_ms` existe no runtime, mas o journal v2 não exporta essa
série por chamada; o protocolo acima fornece tempo agregado. Não inventar essa
decomposição nem subtrair somas de tarefas concorrentes para obter "overhead".
Não ampliar a telemetria do produto nesta consolidação.

Registrar tentativas malsucedidas, interrupções e erros externos. Para repetição,
usar bases novas e registrar n, mediana e mínimo–máximo; não selecionar apenas a
melhor tentativa. Uma execução não prova superioridade competitiva. Sem anterior
adequado, esta avaliação observará correção e escolhas do estado atual, sem provar
ganho comparativo. As fixtures locais anteriores não mediram rodadas reais de modelo;
menos invocações em sequência predeterminada não equivale a menos rodadas reais.
