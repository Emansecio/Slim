# Release do Slim

Este arquivo registra somente o deploy vigente e o procedimento reproduzível.
Deploys anteriores estão em [`history/2026-09.md`](history/2026-09.md).

## Deploy local vigente — revisão de compactação (2026-09-27)

.\refresh-slim.ps1 terminou novamente com exit 0 e OK: Slim slim 0.1.0
após a comparação B1. O build incremental levou 0,25 s e reutilizou o mesmo
binário, pois B1 e B2 adicionaram apenas testes. O build release inicial levou
354,60 s.
Executável no PATH: C:\Users\Thiago Emanuel\bin\Slim.exe, 19.590.144 bytes,
build 2026-09-27T01:08:31.9879464-03:00. SHA-256 do instalado e do
target/release/slim.exe: C91EAD9B2E6D7FD118C75B3848010FFD4F632BB1C752F9375BF094775886EAAD.
Revisão do build: edcc6708574b956ab2629e90351ae44dab8416e4-dirty.
Get-Command Slim apontou para essa cópia e --version terminou com exit 0.

O leitor HTTP do Jev agora aplica o limite existente de 1 MiB enquanto recebe
o corpo. O teste com stream localhost excessivo falhou antes e passou após a
correção; os 29 testes Jev passaram. Após adicionar os testes integrados B1/B2
da TUI, test-slim.ps1 -Workspace terminou com exit 0 em 74,39 s: 80 alvos,
2.130 testes aprovados, 39 ignorados e zero falhas. Clippy -D warnings passou
em cada crate separadamente com --no-deps; rustfmt e diff-check dos testes novos
passaram. O worktree inclui alterações não commitadas preexistentes, preservadas
no build.

Não houve chamada comercial, medida de cache faturado, latência de inferência
de resumo ou console físico. A medição offline da TUI confirmou que a
preparação B2 já estava habilitada. A comparação B1 preservou 5/6 fatos no
extrato local e 6/6 no resumo HTTP simulado, que cobrou mais tokens estimados
e atrasou o request de tarefa. O padrão local foi mantido.
Os dados e as propostas de política constam no artefato de revisão da categoria
4; nenhuma mudança de limiar, prompt, modelo ou destino de dados do Jev foi feita.

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
