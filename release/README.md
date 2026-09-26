# Release do Slim

Este arquivo registra somente o deploy vigente e o procedimento reproduzível.
Deploys anteriores estão em [`history/2026-09.md`](history/2026-09.md).

## Deploy local vigente — cinco correções de resiliência (2026-09-24)

`.\refresh-slim.ps1` terminou com exit 0 e `OK: Slim slim 0.1.0`.
Executável no PATH: `C:\Users\Thiago Emanuel\bin\Slim.exe`, **19.850.240 bytes**,
build **2026-09-24T23:31:21.0977596-03:00**. SHA-256 instalado e release
idênticos: `F011A9D0715BBBC5CA382008008AE84BEFB3A0B7AFD54ABF0412B03E58DEE877`.
Revisão: `6a8e66b306d298ce564af4a2ae851e5212d6151a-dirty`.
`Get-Command slim`, `--version` (exit 0) e comparação de hashes confirmaram
identidade e publicação. Build release padrão: 376,81 s pelo wrapper.

OAuth mantém a credencial renovada durante falha de persistência, reconcilia
sem novo POST e coordena refresh entre processos Windows. A preferência opcional
de método respeita seleção explícita sem apagar a credencial alternativa.
A preparação TUI é cancelável, correlacionada por identidade e recupera o prompt
sem replay automático. Busca informa cobertura parcial por falhas I/O.
MCP distingue interrupção anterior ao envio de efeito remoto incerto, encerra
recursos e preserva output terminal, inclusive durante cancelamento com fila cheia.
Contratos na [referência de runtime](../docs/reference/CLI-AND-RUNTIME.md) e no
[design da TUI](../docs/DESIGN-SLIM-TUI.md).

Validação direcionada nesta sessão: CLI lib 149/149, TUI lib 284/284,
prompt_admission 4/4, tui_bridge 25/25, integração TUI/MCP 1/1 e compatibilidade
do snapshot v1 1/1. Integrações finais: oauth_contract 35/35, headless_resume
12/12, session_continuation 5/5 e sec_secret_flow 2/2. MCP: stdio 13/13,
mcp_manager 16 aprovados e 2 fixtures auxiliares ignoradas, runtime_abort 10/10;
teste bounded do output terminal 1/1. Busca: unitários 29/29, search_bounded 9/9
e tool_contracts 30/30. Agent_loop passou 104/104 no checkpoint MCP.
Revisões independentes finais sem bloqueadores; rustfmt dirigido e
`git diff --check` passaram. Os resultados não representam uma execução de toda
a suíte workspace. A publicação sem `-Test` compilou release e passou no smoke.

Não houve login OAuth comercial nem validação em console físico. A ordem dos
avisos finais após restauração do terminal foi conferida no código. Cancelar
HTTP não garante parada/rollback remoto; cleanup pode ser `Unconfirmed`.
Crash após rotação remota e antes de persistir o token pode exigir novo login.
Arquivos com `preferred_method` são incompatíveis com binários antigos de schema
estrito. A exclusão entre processos OAuth foi validada no Windows.

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
