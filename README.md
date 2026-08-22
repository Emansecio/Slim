# Slim

> **âš ï¸ Agentes de cÃ³digo:** leiam [RULES.md](RULES.md) e [AGENTS.md](AGENTS.md)
> **antes de qualquer tarefa**. Regra Zero: toda mudanÃ§a de cÃ³digo termina com
> `.\refresh-slim.ps1` imprimindo `OK:` â€” o `slim` do PATH Ã© uma cÃ³pia estÃ¡tica
> e nÃ£o acompanha o cÃ³digo sozinho. Proibido afirmar "feito/testado" sem
> evidÃªncia executada na sessÃ£o.

Harness de coding agent em Rust para Windows x64/MSVC (`0.1.0`). **Status de
implementaÃ§Ã£o: checkpoint de integraÃ§Ã£o parcial, nÃ£o v1 concluÃ­da.** Headless e
TUI fullscreen usam provider, agent loop e tools reais; a TUI recebe streaming,
usage e cancelamento por channels tipados, sem executar domÃ­nio.

## Build e deploy do binÃ¡rio (`slim` no PATH)

O comando `slim` do terminal aponta para `C:\Users\User\bin\Slim.exe`, uma
**cÃ³pia estÃ¡tica** â€” ela **nÃ£o** acompanha o cÃ³digo automaticamente. Depois de
qualquer mudanÃ§a, rode:

```powershell
.\refresh-slim.ps1          # build release + copia para o PATH + smoke test
.\refresh-slim.ps1 -Test    # idem, rodando cargo test --workspace antes
```

Deploy manual equivalente:

```powershell
cargo build --release -p slim-cli
Copy-Item target\release\slim.exe C:\Users\User\bin\Slim.exe -Force
```

**Agentes de cÃ³digo devem rodar `.\refresh-slim.ps1` antes de encerrar
qualquer tarefa que altere cÃ³digo** â€” ver [AGENTS.md](AGENTS.md). O script
tambÃ©m corrige o `RUSTC` da sessÃ£o (as variÃ¡veis de usuÃ¡rio `RUSTC`/`CARGO`
apontam para um caminho inexistente; detalhes no tracker TUI Â§8.3).

## ConfiguraÃ§Ã£o (`slim.toml`)

O Slim lÃª defaults de dois arquivos TOML, mesclados nessa precedÃªncia:

**flag CLI > variÃ¡vel de ambiente > projeto > global > default interno**

| Camada | Caminho |
|---|---|
| Projeto | `./slim.toml` (diretÃ³rio de trabalho) |
| Global | `%APPDATA%\slim\slim.toml` no Windows |

Chaves reconhecidas hoje (desconhecidas sÃ£o ignoradas):

```toml
model = "gpt-4o-mini"
endpoint = "https://api.openai.com/v1/chat/completions"
effort = "high"   # low | medium | high (afeta o label do composer na TUI)
```

- Arquivo ausente Ã© normal; **arquivo presente e invÃ¡lido aborta** com erro
  nomeando o caminho â€” sem silenciar configuraÃ§Ã£o quebrada.
- As camadas sÃ³ entram em jogo quando o modo provider estÃ¡ ativo (flag ou env);
  config sozinha nÃ£o ativa rede.


Os 212 testes verdes em 45 suítes de teste comprovam componentes, contratos e caminhos
headless/TUI offline; nÃ£o comprovam integraÃ§Ã£o completa do produto:

- `cargo fmt --all -- --check`, Clippy workspace com `-D warnings`, `cargo test
  --workspace` e `cargo build --workspace --release`: todos exit code `0`;
- 212 testes offline em 45 suítes, incluindo proptest, fault injection,
  golden matrix via TestBackend e benchmark long-session com gate de budget;
- E2E do binÃ¡rio real contra fixtures localhost, sem chamada a provider live;
- nenhuma matriz fÃ­sica completa de terminal, IME, mouse ou clipboard foi
  executada.

## Status atual de integraÃ§Ã£o

| Ãrea | Estado atual |
|---|---|
| Headless/provider/tools/auth/compaction/usage/artifacts/anti-loop | `Slim --headless` usa caminhos integrados e exercitados offline; OpenAI-compatible/Anthropic SSE, read/list/search/write/patch/shell e filtragem de capabilities funcionam. |
| TUI | `Slim` abre a interface normal mesmo deslogado. `/login` abre seletor OAuth nativo para Claude Pro/Max ou ChatGPT Plus/Pro; Anthropic Messages e Codex Responses usam o mesmo agent loop/tools Slim. Reducer Ãºnico normativo (`Action â†’ reduce â†’ Effect`), scrollback virtualizado e navegÃ¡vel (pin/live-edge/unseen), paleta estratificada Â§21.3, ActivityRail com spinner e medidor de contexto na operational bar, composer minimalista (hairline + label discreto), tool blocks tipados agregados, lanes bounded com coalescer no runtime real, command palette Ctrl+P, markdown-light, motion bÃ¡sico com reduced motion, welcome com wordmark braille e pulso ambiente â‰¤2 fps. Fixtures localhost provam OAuth/callback/store, streaming, tool round-trip e cancelamento. PTY E2E fÃ­sico pendente de console real (teste `#[ignore]`). |
| Cache | Implementado e testado em `HttpProviderClient`, porÃ©m os caminhos normais usam `HttpProviderClient::new`; cache nÃ£o fica ativo no produto normal. |
| SessÃµes | Writer/recovery/branch e `--session` escrevem JSONL novo; nÃ£o hÃ¡ resume, seleÃ§Ã£o de recovery ou UX de branch/fork na CLI/TUI. |
| Skills, MCP, subagentes | MÃ³dulos/contratos/testes existem; startup, catÃ¡logo de tools e loop nÃ£o os conectam ao runtime, e nÃ£o hÃ¡ child agents reais. |
| Todo/Plan/Goal | Estruturas/testes existem, mas nÃ£o sÃ£o tools vivas end-to-end; Plan para em `approval_required` antes de gerar um plano via provider. |
| Release | DeterminÃ­stico e hash-valid; empacota este checkpoint parcial, nÃ£o uma v1 completa. |

O restante desta pÃ¡gina descreve contratos realmente integrados. Para o plano de
integraÃ§Ã£o pendente, consulte o [Ã­ndice canÃ´nico](DocumentaÃ§Ãµes%20-
Projeto/README.md) e o [plano](DocumentaÃ§Ãµes%20-
Projeto/PLANO-IMPLEMENTACAO.md).

## Contratos implementados no headless

No Windows, `auth.json` Ã© somente leitura e fail-closed. O arquivo Ã© aberto por
handle `windows-sys` e recebe DACL protegida com allowlist exata do owner atual,
usuÃ¡rio atual, `SYSTEM` e `Administrators`; o teste tambÃ©m cobre caminho Unicode.
Symlink, diretÃ³rio, reparse point, ACL divergente, JSON/schema invÃ¡lido ou
versÃ£o diferente falham. No headless, arquivo ausente nÃ£o Ã© criado. A TUI pode
escrever credenciais OAuth tipadas no mesmo arquivo por temp exclusivo, ACL e
replace atÃ´mico. NÃ£o hÃ¡ detecÃ§Ã£o mÃ¡gica de segredos desconhecidos.

O cliente HTTP nÃ£o segue redirects. O cache Ã© opcional, em memÃ³ria e por
processo; sua implementaÃ§Ã£o e testes existem, mas a construÃ§Ã£o normal do
produto usa `HttpProviderClient::new`, portanto o cache nÃ£o estÃ¡ ativo no caminho
headless/TUI atual. Quando habilitado, seu namespace inclui provider, identidade do endpoint e modelo exato,
alÃ©m de mensagens/tools e conteÃºdo multimodal canonicalizados. Headers e chaves
ficam fora da chave. Respostas com tool call nunca sÃ£o cacheadas. SSE exige
evento terminal; bytes/eventos apÃ³s a terminaÃ§Ã£o ou stream sem terminaÃ§Ã£o sÃ£o
rejeitados. Os adapters preservam, quando fornecidos, os IDs/index/name de tool
deltas OpenAI e os IDs de `content_block` Anthropic; somente chamadas legadas
sem ID recebem identificador interno. JSON de tool malformado ou incompleto Ã©
rejeitado.

O runtime verifica `max_tool_calls` antes de executar efeitos colaterais,
emite `ToolStarted`, mantÃ©m pares assistant/tool e o prompt raiz atravÃ©s da
compactaÃ§Ã£o bounded no mesmo adapter/modelo, e persiste eventos `Usage` do
resumo e dos turnos. Cada valor de API key fornecido Ã© redigido exatamente antes
de tool output, follow-up ao provider, renderizaÃ§Ã£o e sessÃ£o; isso nÃ£o promete
encontrar qualquer segredo arbitrÃ¡rio que nÃ£o tenha sido registrado.

`--image PATH` aceita repetidamente PNG, JPEG/JPG, GIF e WebP; cada entrada deve
ser arquivo regular nÃ£o-symlink, nÃ£o vazio e ter no mÃ¡ximo 20 MiB. O conteÃºdo Ã©
enviado como base64 estrito, sem I/O remoto. `SLIM_CONTEXT_WINDOW_TOKENS` e
`SLIM_MAX_OUTPUT_TOKENS` aceitam inteiros positivos e controlam janela/reserva
de contexto e o teto de saÃ­da do provider (padrÃ£o de saÃ­da: 4096 tokens). O
resultado de tool tambÃ©m tem cap bounded de 64 KiB, com artifact handle quando
aplicÃ¡vel.

No headless, text e JSONL expÃµem `stop`: `provider_completed` (exit `0`),
`turn_limit` e `repeated_failed_tool` (exit `12`) ou `tool_limit` (exit `22`).
O provider/model/IDs informados sÃ£o preservados; nÃ£o hÃ¡ troca silenciosa de
provider.

## Comece aqui

O Ã­ndice canÃ´nico estÃ¡ em
[DocumentaÃ§Ãµes - Projeto/README.md](DocumentaÃ§Ãµes%20-%20Projeto/README.md).

As decisÃµes normativas estÃ£o em
[DECISOES-GRILL-PRE-IMPLEMENTACAO.md](DocumentaÃ§Ãµes%20-%20Projeto/DECISOES-GRILL-PRE-IMPLEMENTACAO.md)
e o plano/evidÃªncia final em
[PLANO-IMPLEMENTACAO.md](DocumentaÃ§Ãµes%20-%20Projeto/PLANO-IMPLEMENTACAO.md) e
[POC-RESULTS.md](POC-RESULTS.md).

## Provider headless e autenticaÃ§Ã£o local

Sem configuraÃ§Ã£o, o CLI usa o provider fake dos testes. A rota HTTP local
suporta `openai-compatible` e `anthropic`; a validaÃ§Ã£o registrada usa somente
fixtures localhost, sem credencial real ou rede externa.

As chaves sÃ£o resolvidas nesta ordem: `SLIM_API_KEY`, variÃ¡vel especÃ­fica do
provider (`OPENAI_API_KEY` ou `ANTHROPIC_API_KEY`), `SLIM_AUTH_FILE` e
`%USERPROFILE%\\.slim\\auth.json`. O formato aceito Ã©:

```json
{
  "version": 1,
  "providers": {
    "openai-compatible": { "api_key": "..." },
    "anthropic": { "api_key": "..." }
  }
}
```

O headless nÃ£o grava `auth.json`. A TUI grava somente credenciais obtidas por
`/login`, preservando entradas API-key existentes. Access/refresh/code nunca
entram em events ou sessÃµes. `--session PATH` opta por persistir os eventos
JSONL do turno headless; sem essa flag nÃ£o hÃ¡ sessÃ£o headless.

## TUI e OAuth nativo

`Slim` sempre abre a tela normal com composer. Sem credencial, status mostra
`signed out Â· type /login`; prompt comum Ã© preservado e recebe aviso local.

- `/login`: seletor Anthropic Claude Pro/Max ou OpenAI Codex ChatGPT Plus/Pro;
- autocomplete: digitar `/` em qualquer posiÃ§Ã£o do prompt (inÃ­cio ou meio de
  frase) abre as opÃ§Ãµes; â†‘/â†“ navegam, **Tab** completa no draft, **Enter**
  completa e executa, `Esc` fecha;
- `/login anthropic` e `/login codex`: atalhos diretos;
- `/logout`: remove credencial OAuth ativa;
- `/model`: seletor GPT-5.6 Sol/Terra/Luna para Codex;
- `/model sol`, `/model terra`, `/model luna`: aliases diretos;
- `Esc`: cancela seletor/login em andamento.

OAuth usa PKCE S256, callback restrito a loopback, validaÃ§Ã£o de `state`, refresh
e browser via `ShellExecuteW` sem shell. Codex usa SSE Responses; WebSocket fica
fora deste checkpoint. Testes sÃ£o localhost/offline: nenhum login real foi
executado, e polÃ­tica de limites/cobranÃ§a pertence aos providers.
