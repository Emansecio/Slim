# Release do Slim

Pacote Windows x64/MSVC do Slim `0.1.0`. O ZIP Ã© determinÃ­stico e contÃ©m
exatamente uma entrada, `slim.exe`; nÃ£o inclui `auth.json`, `.slim`, sessÃµes,
artefatos, prompts, documentaÃ§Ã£o ou segredos. Os hashes publicados ficam em
`manifest.json` e `SHA256SUMS.txt`.

> **Status do pacote (2026-08-21):** os artefatos anteriores
> (`slim-0.1.0-windows-x64.zip`, `manifest.json`, `SHA256SUMS.txt`) foram
> **removidos** por serem de um binÃ¡rio anterior aos Ãºltimos slices do tracker
> TUI. Regere o pacote com `python3 release/build_release.py` a partir de um
> `target/release/slim.exe` fresco antes de distribuir.

> **Status de implementaÃ§Ã£o:** este artefato Ã© o checkpoint `0.1.0` de
> integraÃ§Ã£o parcial. `Slim` abre a TUI normal mesmo deslogado; `/login` conecta
> OAuth nativo Claude Pro/Max ou ChatGPT Plus/Pro. `Slim --headless` usa o caminho
> headless; ambos usam provider/loop/tools reais. A TUI inclui composer,
> streaming, usage e cancelamento. Cache normal, Skills,
> MCP, subagentes, resume/branch de sessÃ£o e Todo/Plan/Goal nÃ£o estÃ£o ligados
> integralmente ao runtime normal. O pacote nÃ£o Ã©
> uma v1 completa.

## VerificaÃ§Ã£o reproduzÃ­vel

Na raiz do workspace, com `target/release/slim.exe` jÃ¡ compilado:

```powershell
python3 release/build_release.py
python3 release/build_release.py
sha256sum -c release/SHA256SUMS.txt
7z l release/slim-0.1.0-windows-x64.zip
.\target\release\slim.exe --version
.\target\release\slim.exe --help
.\target\release\slim.exe --headless --unknown-option
```

As duas execuÃ§Ãµes devem produzir o mesmo SHA-256 do ZIP. A listagem deve
mostrar somente `slim.exe`, com timestamp fixo `1980-01-01 00:00:00`. O builder
usa apenas a biblioteca padrÃ£o do Python, resolve todos os caminhos a partir da
raiz de `build_release.py`, escreve somente em `release/` e nÃ£o cria artefato
temporÃ¡rio fora de `release/`.

O binÃ¡rio deve retornar exit `0` para `--version` e `--help`, e exit `30` para
opÃ§Ã£o desconhecida. A verificaÃ§Ã£o de bytes/nome do ZIP nÃ£o encontra os markers
secretos das fixtures.

## Escopo e limitaÃ§Ãµes

O pacote registra 225 passed / 0 failed / 1 ignored em 45 suítes e um E2E do binário real
contra provider localhost. NÃ£o houve chamada a provider live nem validaÃ§Ã£o
fÃ­sica completa de terminal, IME, mouse ou clipboard.

Esses testes comprovam componentes/contratos, headless e a bridge TUI central,
nÃ£o a integraÃ§Ã£o de todas as capabilities da v1. O cache implementado Ã© testado, mas nÃ£o Ã©
ativado pelos construtores normais (`HttpProviderClient::new`).

`auth.json` continua opcional, somente leitura e fail-closed. No Windows, a
implementaÃ§Ã£o usa handle `windows-sys` e DACL protegida com allowlist exata do
owner atual, usuÃ¡rio atual, `SYSTEM` e `Administrators`, incluindo teste de
caminho Unicode. Symlink, arquivo nÃ£o regular, ACL divergente ou schema invÃ¡lido
falham; arquivo ausente nÃ£o Ã© criado. Credenciais nÃ£o entram em cache, sessÃ£o,
manifest ou ZIP.
