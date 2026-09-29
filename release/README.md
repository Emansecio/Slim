# Release do Slim

Este arquivo registra somente o deploy vigente e o procedimento reproduzível.
Deploys anteriores estão em [`history/2026-09.md`](history/2026-09.md).

## Deploy local vigente — experiência de uso da TUI (2026-09-28)

`.\refresh-slim.ps1` terminou com exit 0 e `OK: Slim slim 0.1.0`.
Build release padrão: 234,73 s pelo wrapper, com um job Cargo.
Executável no PATH: `C:\Users\Thiago Emanuel\bin\Slim.exe`, 20.132.352 bytes,
build UTC `2026-09-29T00:41:14Z`. SHA-256 do instalado e do
`target/release/slim.exe` idênticos:
`60AE6FAD3FE80B2EA38D4FF74956A178DB28FE89619D000DAF30E34FD227EDAF`.
Revisão do build: `e248c687785f4e101cd3e0bebde809b9311739a2-dirty`.
`Get-Command Slim` confirmou essa cópia; `--version` terminou com exit 0.
As alterações preexistentes do checkout (inclusive de outras frentes em andamento)
foram preservadas e incluídas no build.

Muda a TUI e três pontos do core, sem timer novo. Contrato em
[§1.2 do DESIGN](../docs/DESIGN-SLIM-TUI.md), parágrafos "Experiência de uso":

- **Presença desde o envio:** `● Slim` aparece sob o prompt no instante do envio e
  vira o cabeçalho do primeiro bloco do agente sem deslocar nada.
- **Recibo de fim de turno:** `✓ 3 arquivos · +42 -7 · 2 comandos · 6s   Ctrl+D`.
- **Entrada:** `↑`/`↓` recuperam prompts (com composer vazio no live edge; `PageUp`
  fixa a viewport e daí `↑`/`↓` navegam blocos como antes), `@arquivo` com busca
  aproximada e anexo do conteúdo (8 arquivos, 256 KiB cada, 1 MiB no total; nome de
  segredo, binário e caminho fora do workspace recusam o prompt), `!comando` no
  shell do usuário só no modo Auto, pela ferramenta `shell` do agente.
- **Sessões:** `/resume` com lista, título, idade e filtro; `/rename` (arquivo
  `.meta.json` ao lado da sessão); `/rewind` forka a sessão antes de um turno
  concluído, troca para o fork e devolve o prompt ao composer. Só a conversa volta,
  arquivos alterados não são restaurados; a sessão original fica intacta.
- **Progresso de chamada longa:** `Preparando edição · 4,2 KB` enquanto o modelo
  escreve uma chamada grande. Antes, o `PreparingTool` só chegava ao fim do
  stream (medido com um SSE segurado por 1,5 s).

Pontos de segurança tocados, todos aditivos: o `ProviderEventRedactor` agora conta
bytes de argumentos e emite `ProviderEvent::ToolCallProgress { name, bytes }` (nome
redigido, sem conteúdo; a retenção do conteúdo e o portão de segredo em `Stopped`
não mudaram; teste com credencial registrada na chamada em escrita); leitura de
arquivo por `@` usa `resolve_workspace_path_from_root` com limites próprios; `!`
usa o gate de modo do core sem alterá-lo; o fork de `/rewind` cria uma sessão nova
com `id == stem` e não toca o original. `resolve_workspace_path*`,
`workspace_sessions_dir`, `ensure_resume_preflight`, `allows_mutation` e
`names_for_mode` não foram alterados.

Checks desta rodada (execuções feitas antes do build):

- `cargo test --offline -p slim-tui`: lib 362 e integração 287 aprovados, 0 falhas.
- `slim-cli` (`test-slim.ps1 -Lib -TestTarget tui_bridge,tui_runtime,session_continuation,tui_pty,ask_question_tui`):
  lib 186, ask_question_tui 4, session_continuation 5, tui_bridge 27, tui_pty 4
  (1 ignorado, exige ConPTY real), tui_runtime 3; todos aprovados.
- `slim-core`: lib 360 aprovados (13 ignorados) e os alvos de integração, incluindo
  os novos `provider_tool_progress` (2) e `session_turns`/`workspace_files`. **Uma
  falha:** `agent_loop::provider_recovery_retries_headers_timeout_when_no_tools_ran`
  (timeout de headers esgota a recuperação automática em 2/2). Ela está em
  `tests/agent_loop.rs`, arquivo com alterações não commitadas de outra frente, e
  trata de recuperação de conexão, não de chamadas de ferramenta; não a investiguei
  contra o HEAD, então não afirmo que seja anterior a esta mudança.
- `cargo clippy --offline -p slim-tui -p slim-cli --all-targets` e
  `-p slim-core --lib --test provider_tool_progress --test provider_http`, com
  `-D warnings` permitindo `manual_clamp` e `bool_to_int_with_if`: exit 0.
  rustfmt dos três crates e `git diff --check` passaram.

Não foram executados: a suíte completa do workspace, `slim-lsp`, o console físico
(ConPTY real), uma sessão com provider real, `!`/`@`/`/rewind` num terminal de
verdade (só via estado, quadros renderizados e o worker com providers de teste), o
movimento (varredura, caret) em terminal físico, e a sonda de lock de sessão em
Unix (só o caminho de Windows foi exercitado). O agente que implementou o lado host
das sessões rodou os mesmos alvos do `slim-cli` e o clippy do crate; os números
acima são da minha execução final.

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
