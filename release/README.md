# Release do Slim

Este arquivo registra somente o deploy vigente e o procedimento reproduzível.
Deploys anteriores estão no [histórico](history/README.md).

## Deploy vigente — Organização e polimento da TUI (02/10/2026)

`.\refresh-slim.ps1` terminou com exit 0, build release padrão em 468,93 s,
e `OK: Slim slim 0.1.0 implantado em C:\Users\Thiago Emanuel\bin\Slim.exe`.
O comando `Slim` resolve esse executável e `--version` retornou `slim 0.1.0`, exit 0.
Identidade verificada: 21.388.288 bytes, build de
2026-10-02 18:54:36 -03:00 (America/Sao_Paulo).
SHA-256 idêntico ao de `target/release/slim.exe`:
`048B3AABA26E5509FC44138BFF2E04B0A0B93C2EEAAE716165A7C49363AC6739`.
Revisão compilada: `848b325c65dc838d758a5b9066071e54fb8b8bf9-dirty`.
O build inclui o checkout com suas alterações preexistentes.

- Grupos de ferramentas abrem os resumos; cada membro expande seus detalhes
  individualmente, com paginação e âncoras estáveis. Thinking concluído entre
  ferramentas permanece acessível dentro do grupo.
- O único cabeçalho `● Slim` por turno foi mantido, com a ordem original da
  resposta e das ferramentas.
- Todo começa compacto, indica bloqueios e preserva a preferência manual de expansão.
- Textos consecutivos na fila ficam recolhidos sob uma prévia expansível.
  O contador continua no composer; edição, remoção e envio seguem a ordem da fila.
- `/` e Ctrl+P usam as mesmas categorias e ordem, com títulos fora da seleção.
  Comandos, skills e navegação foram verificados também em viewport estreito.

Validação desta entrega:

| Comando | Resultado |
|---|---|
| `.\test-slim.ps1 -Package slim-tui -Lib -TestTarget integration` | exit 0; 387 testes de biblioteca e 303 de integração aprovados |
| `.\test-slim.ps1 -Package slim-tui -TestTarget properties,prompt_admission,tui_interaction_focus,pty_windows` | exit 0; 37 aprovados |
| `.\test-slim.ps1 -Package slim-cli -TestTarget tui_bridge,session_continuation` | exit 0; 45 aprovados |
| `cargo test --jobs 1 --timings -p slim-tui --test visual_snapshots -- --ignored --test-threads 1` | exit 0; 1 aprovado; snapshots do renderer de produção inspecionados offline |
| `cargo clippy --jobs 1 -p slim-tui --all-targets -- -A clippy::bool_to_int_with_if -A clippy::manual_clamp` | exit 0, sem warnings; allowances preexistentes preservados |

Total: 773 testes focados aprovados, 0 falhas. `rustfmt --edition 2021
--config skip_children=true --check` passou nos 16 arquivos tocados;
`git diff --check` e os links relativos da documentação afetada foram verificados.
Os testes cobriram paginação individual, diffs, Thinking entre ferramentas,
âncoras de rolagem, hold de 249 ms, ordem da fila e seleção nos menus agrupados.
A inspeção visual incluiu tarefas em 40×8, fila aberta/recolhida, detalhes de
ferramentas, slash e paleta estreita.

Limites: a suíte completa do workspace não foi repetida nesta entrega. A
inspeção visual usou TestBackend e imagens offline das células dos snapshots;
console físico e sessão com provider externo não foram exercitados.
O alvo `pty_windows` verifica eventos de entrada e não comprova ConPTY real.

Contrato visual: [DESIGN da TUI](../docs/DESIGN-SLIM-TUI.md).
A validação e o deploy anteriores estão no [histórico de outubro](history/2026-10.md).
Não houve commit, push, instalação de dependências ou alteração global de configuração.

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
