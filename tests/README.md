# Testes de integração do workspace

Estes testes exercitam contratos entre `slim-core`, `slim-tui` e `slim-cli`.
Embora estejam na raiz, são registrados explicitamente em
`crates/slim-cli/Cargo.toml` por entradas `[[test]]` com caminhos relativos.

Execute-os pelo workspace ou pelo pacote CLI:

```powershell
cargo test --workspace
cargo test -p slim-cli --test smoke_workspace --test e2e_v1 --test e2e_offline
```

Fixtures e suporte compartilhado permanecem em `tests/fixtures/` e
`tests/support/`.

## Integrações LSP

Os alvos de `slim-lsp` preservam fixtures stdio separadas dos gates que exigem
servidores de linguagem externos:

| Alvo | Contrato exercitado |
|---|---|
| [mock_subprocess](../crates/slim-lsp/tests/mock_subprocess.rs) | Pool, processos reais da fixture, leases, cancelamento e projeção semântica |
| [incremental_sync](../crates/slim-lsp/tests/incremental_sync.rs) | Unicode/CRLF, alterações externas, inputs Cargo e gerações |
| [post_edit_diagnostics](../crates/slim-lsp/tests/post_edit_diagnostics.rs) | Linha de base, publicação exata, cobertura e limites pós-edição |
| [typescript](../crates/slim-lsp/tests/typescript.rs) | Defaults automáticos, oito extensões, raiz autorizada, filtro de servidor e lotes mistos |
| [real_typescript](../crates/slim-lsp/tests/real_typescript.rs) | Gate explícito com TypeScript Language Server real: navegação, diagnósticos, projetos aninhados e refresh de JSONC/arquivos |
| [real_incremental](../crates/slim-lsp/tests/real_incremental.rs) | Gate explícito com rust-analyzer real e utilidade das consultas semânticas |

```powershell
.\test-slim.ps1 -Package slim-lsp -TestTarget mock_subprocess,incremental_sync,post_edit_diagnostics,typescript
# Gate ignorado por padrao; requer dependencias externas ja instaladas.
cargo test -p slim-lsp --test real_typescript -- --ignored
# Rust real, com rust-analyzer disponivel no PATH do processo.
cargo test -p slim-lsp --test real_incremental -- --ignored
```

O gate `real_typescript` exige Node.js, TypeScript Language Server e TypeScript
compatíveis, sem instalá-los nem alterar PATH. A variável opcional
`SLIM_TEST_TYPESCRIPT_LANGUAGE_SERVER` aceita o executável ou entrypoint Node
de uma instalação existente; ela só seleciona o servidor do teste. Esses testes
não representam configuração obrigatória ou ativação no Slim. O alvo
`typescript` usa a fixture controlada e não comprova compatibilidade com o
servidor externo. A execução e seus resultados devem ser registrados no gate
correspondente, sem inferir sucesso pela existência dos arquivos.

O gate real exige o `tsserver.js` clássico. Publicações sem versão conservam
`diagnostic_version=null` e completude desconhecida; pós-edição exige
`Unverified`. Versões executadas e evidências ficam em
[release/README.md](../release/README.md).

## Ciclo local com carga controlada

Use [`test-slim.ps1`](../test-slim.ps1) para selecionar explicitamente os alvos:

```powershell
# Unidade do fluxo alterado; o filtro nao substitui a selecao de alvo.
.\test-slim.ps1 -Package slim-core -Lib -Filter runtime::shell_jobs
# Integracoes afetadas, em uma unica invocacao.
.\test-slim.ps1 -Package slim-core -TestTarget agent_loop,native_tool_recovery
# CodeMode: isolamento/persistencia e composicao com MCP HTTP/stdio locais.
.\test-slim.ps1 -Package slim-core -Lib -Filter runtime::codemode
.\test-slim.ps1 -Package slim-core -TestTarget codemode
# Providers e catalogos de ambos os pacotes, com dependencias resolvidas juntas.
.\test-slim.ps1 -Package slim-core,slim-cli -TestTarget providers,catalogs
# Um modulo do alvo agrupado, sem executar os demais casos.
.\test-slim.ps1 -Package slim-core -TestTarget providers -Filter opencode_go_provider::
# Contratos entre crates.
.\test-slim.ps1 -Package slim-cli -TestTarget smoke_workspace,e2e_v1,e2e_offline
# Mudancas abrangentes: inclui todos os testes padrao do workspace e doc-tests.
.\test-slim.ps1 -Workspace
```

Escolha os alvos pelo comportamento e pelas dependencias afetadas; os exemplos
nao constituem cobertura suficiente para toda alteracao. O script exige `-Lib`
ou `-TestTarget` no modo direcionado e interrompe em qualquer falha do Cargo.
`-Package` aceita varios pacotes para validar uma mudanca entre crates em uma
unica chamada. `tokio`, `windows-sys` e as dev-dependencies `proptest` e
`portable-pty` vem de `[workspace.dependencies]` com a uniao de features, para
que qualquer selecao (`-p` ou `-Workspace`) reutilize os mesmos artefatos de
`slim-core` e das dependencias. Ao adicionar uma feature a uma dessas
dependencias, adicione-a nessa uniao; confira com `cargo tree -e features`.

O wrapper aponta `SLIM_CONFIG_FILE` e `SLIM_AUTH_FILE` para caminhos temporarios
vazios e remove do processo as variaveis de autenticacao reconhecidas pelo CLI.
Ao terminar, inclusive em falha, restaura o ambiente e remove o diretorio
temporario. Fixtures podem definir suas proprias credenciais/configuracoes.
Isso nao isola rede, configuracao do workspace, skills globais ou outros
overrides `SLIM_*`; Cargo direto tambem nao aplica esse isolamento.

Os 11 antigos alvos de parsing, adapters, catalogos e protocolo de `slim-core`
agora sao modulos de [`providers`](../crates/slim-core/tests/providers/main.rs).
Os quatro catalogos do CLI compartilham
[`catalogs`](../crates/slim-cli/tests/catalogs/main.rs). Os casos e suas assertivas
foram preservados; use `-TestTarget providers`/`catalogs` e `-Filter modulo::`
para selecionar um arquivo antigo. Testes de transporte/tempo, subprocessos e
aqueles que alteram variaveis de ambiente conservam seu isolamento existente.

Oito alvos de contratos em memoria do `slim-core` compartilham
[`contracts`](../crates/slim-core/tests/contracts/main.rs). Use
`-TestTarget contracts` e `-Filter modulo::` para selecionar um conjunto antigo;
as assertivas continuam nos mesmos arquivos, agora dentro desse modulo.

Dezesseis alvos de armazenamento, recuperacao e reducer de sessao do
`slim-core` compartilham [`sessions`](../crates/slim-core/tests/sessions/main.rs);
use `-TestTarget sessions` e `-Filter modulo::` da mesma forma. No CLI, seis
fluxos em processo sobre fixtures loopback (`adv_cli_args`, `ask_question_tui`,
`headless_resume`, `opencode_go_headless`, `sec_secret_flow`, `tui_runtime`)
compartilham [`flows`](../crates/slim-cli/tests/flows/main.rs). Cada executavel
de teste custa um link separado com um job de compilacao; agrupe alvos novos
sem subprocessos, tempo real ou ambiente proprio no grupo do seu dominio.

O perfil de testes usa `debug=1` nos pacotes locais: mantem nomes, arquivos e
linhas para backtraces, mas omite informacao detalhada de tipos/variaveis.
Dependencias externas usam `debug="line-tables-only"` no perfil dev, herdado
pelos testes: backtraces mantem simbolos e linhas, e os PDBs de cada executavel
de teste encolhem (por exemplo, `rt_session_lock` de 55 MB para 12 MB).
Para depurar variaveis dos pacotes locais, acrescente `-FullDebug`; a troca
recompila os alvos afetados. O perfil release mantem as configuracoes anteriores.

Os testes unitarios de runtime, assim como os de `headless`, `tui`,
`main`, `reducer` e runtime da TUI, ficam em arquivos de modulos carregados
somente com `cfg(test)`. Esses arquivos nao sao dependencias Rust do release;
mantenha novos casos nesses modulos para evitar recompilar producao por uma
mudanca de assertiva. A revisao embutida (`SLIM_BUILD_REVISION`, lida por
`option_env!`) so existe durante o build release de `refresh-slim.ps1`, e
`test-slim.ps1` a remove do ambiente dos testes; assim, publicar ou mudar o
estado clean/dirty nao recompila os alvos de teste. Toolchain, perfil ou
dependencias ainda podem exigir recompilacao.

O servidor de `sec_secret_flow` recebe um sinal quando a chamada sincrona
testada termina e confere as conexoes ja enfileiradas antes de encerrar.
O deadline continua protegendo falhas, e a espera por conexoes nao ocupa CPU
em um loop de `yield_now`.

Os scripts usam um job de compilacao, duas threads de testes por padrao e
prioridade BelowNormal, restaurada ao sair. `-TestThreads` permite ajustar o
limite do harness; threads e subprocessos criados pelos testes nao sao limitados
por esse parametro. A configuracao local tambem define `RUST_TEST_THREADS=2`
para Cargo direto, respeitando uma variavel explicitamente definida pelo usuario.
Prioridade reduzida favorece outros aplicativos, mas nao limita CPU/memoria nem
garante execucao mais rapida. Execute uma operacao Cargo por vez neste checkout.

Apos aprovar os checks necessarios, execute `refresh-slim.ps1` uma vez. Use
`refresh-slim.ps1 -Test` quando ainda precisar da suite completa; nao repita
essa suite imediatamente antes. Nao acrescente `--no-run` antes de executar
os mesmos testes sem uma necessidade intermediaria. Preserve o cache `target`.

Para iteracoes locais em release, `refresh-slim.ps1 -FastBuild` usa `opt-level=3`,
sem LTO e com oito unidades de geracao de codigo. O smoke e a copia para o PATH
continuam iguais, mas o binario fica maior e seu desempenho em uso nao foi
comparado. O comando sem a opcao conserva o perfil padrao; alternar perfis pode
recompilar o executavel. Use o padrao para publicar o build plenamente otimizado.

Os scripts mostram tempo total de cada comando e habilitam `--timings`, cujo
HTML fica em `target/cargo-timings/`. O relatorio Cargo mede compilacao; a saida
do harness informa execucao dos testes. Compare o mesmo alvo, toolchain e estado
de cache; nao interprete compilacao fria versus quente como ganho da alteracao.
Use esses dados antes de agrupar executaveis de teste ou alterar perfis. O perfil
release continua com as otimizacoes existentes e e gerado somente na publicacao.
