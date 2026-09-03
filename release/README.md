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
> OAuth nativo Claude Pro/Max ou ChatGPT Plus/Pro e chave mascarada OpenCode Go.
> `Slim --headless` usa o caminho headless; ambos usam provider/loop/tools reais.
> No headless text, `--verbose` acrescenta uma timeline redigida de tools;
> não pode ser combinado com `--jsonl`.
> OpenCode Go roteia 23 modelos documentados por Chat Completions, Responses ou
> Messages e usa catálogo público bounded com cache/fallback offline. O harness v2 inclui resume/
> recovery explÃ­citos e `RuntimeCapabilityBridge` durÃ¡vel para Skill, MCP local
> selecionado, child e Todo/Plan/Goal. O pacote ainda nÃ£o registra automaticamente
> essas capabilities no provider loop/CLI/TUI, nÃ£o inclui transporte MCP externo,
> processo Skill/child real ou uma v1 completa.

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

O pacote registra 1030 passed / 0 failed / 1 ignored em 87 suítes; a fixture
localhost exercita o turno pela API TUI, sem provider live. O smoke ConPTY do
binário implantado foi executado, mas este host emitiu só o probe `ESC[6n`, sem
frame; a validação física completa de terminal, IME, mouse e clipboard permanece
não observada.

No deploy atual, `target\release\slim.exe` e `C:\Users\User\bin\Slim.exe` têm
18.272.768 bytes e SHA-256 idêntico
`32D629C3B4B98D73ABFAACD4EA0A41F371BD7A36E2F88DCAB32E0FA15C3294FE`;
`slim --version` retorna `slim 0.1.0` com exit `0`.

Esses testes comprovam componentes/contratos, headless, a bridge TUI central e o
capability bridge offline, nÃ£o a integraÃ§Ã£o de todas as capabilities da v1. O cache bounded e o transporte HTTP
compartilhado estÃ£o ativos nos construtores normais; adapter e autenticaÃ§Ã£o seguem isolados por request.

`auth.json` continua opcional e fail-closed para leitura; a TUI grava/remove a
entrada OpenCode Go por temp exclusivo, lock, ACL e replace atômico, preservando
providers irmãos. Precedência: `SLIM_API_KEY` > `OPENCODE_API_KEY` > arquivo. No Windows, a
implementaÃ§Ã£o usa handle `windows-sys` e DACL protegida com allowlist exata do
owner atual, usuÃ¡rio atual, `SYSTEM` e `Administrators`, incluindo teste de
caminho Unicode. Symlink, arquivo nÃ£o regular, ACL divergente ou schema invÃ¡lido
falham; headless não cria arquivo ausente, enquanto login TUI explícito pode
criá-lo com segurança. Credenciais nÃ£o entram em cache, sessÃ£o,
manifest ou ZIP.
