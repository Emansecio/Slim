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
effort = "high"   # low | medium | high (label TUI + reasoning_effort no provider)
max_turns = 128
max_mutating_tool_calls = 32
max_read_tool_calls = 96
max_output_tokens = 4096
timeout_secs = 120
max_result_bytes = 16384

[compaction]
enabled = true
background = true
keep_recent_tokens = 20000
summary_max_bytes = 65536
manual_instructions_max_bytes = 4096
```

- Arquivo ausente Ã© normal; **arquivo presente e invÃ¡lido aborta** com erro
  nomeando o caminho â€” sem silenciar configuraÃ§Ã£o quebrada.
- As camadas sÃ³ entram em jogo quando o modo provider estÃ¡ ativo (flag ou env);
  config sozinha nÃ£o ativa rede.

Tuning de velocidade (env vence o TOML): SLIM_MAX_TURNS, SLIM_MAX_MUTATING_TOOL_CALLS,
SLIM_MAX_READ_TOOL_CALLS, SLIM_MAX_OUTPUT_TOKENS, SLIM_TIMEOUT_SECS, SLIM_MAX_RESULT_BYTES.
Defaults: turns 128, mutating 32/turno, read 96/turno, output 4096, timeout 120s, result 16 KiB.
Reduzir timeout_secs da fail-fast em rede lenta; reduzir max_turns/max_*_tool_calls encurta turnos longos.

## OpenCode Go

O provider `opencode-go` usa chave em `SLIM_API_KEY`, depois
`OPENCODE_API_KEY`, depois `auth.json`. Exemplo headless:

```powershell
$env:OPENCODE_API_KEY = "..."
slim --headless --provider opencode-go --model deepseek-v4-flash --prompt "hello"
```

Na TUI, `/login` oferece **OpenCode Go** com campo mascarado e persistência
atômica; ao reiniciar, o provider API-key ativo é restaurado automaticamente.
Credenciais OAuth continuam no fluxo OAuth, com refresh preservado; arquivo de
autenticação inválido produz erro explícito em vez de aparentar logout.
`/models` abre imediatamente e atualiza em background o catálogo
público `https://opencode.ai/zen/go/v1/models`. O último catálogo válido fica
em `%USERPROFILE%\.slim\opencode-go-models.json`; offline, Slim usa cache ou
registro embutido. Somente os 24 modelos documentados são aceitos. Cada modelo
seleciona protocolo, contexto, reasoning e suporte a imagem por metadado
explícito, sem heurística: Chat Completions, Responses ou Anthropic Messages.
No wire Chat Completions, snapshots cumulativos de `usage` do OpenCode Go são
consolidados em um único total conservador; providers irmãos continuam com
validação terminal estrita.
`/logout` remove apenas a chave OpenCode persistida; variáveis de ambiente não
são alteradas.

Os 1087 testes passados / 0 failed em 87 suítes, mais 1 ignored (ConPTY físico), comprovam
componentes, contratos e caminhos headless/TUI offline; nÃ£o comprovam
integraÃ§Ã£o completa do produto:

Relatório persistente da auditoria iterativa: [WORKFLOW-LOOP-BUGS-SLIM.md](analysis_outputs/WORKFLOW-LOOP-BUGS-SLIM.md).

Auditoria e otimização da suíte: [TEST-SUITE-OPTIMIZATION.md](analysis_outputs/TEST-SUITE-OPTIMIZATION.md).

- `cargo test --workspace` e `refresh-slim.ps1 -Test`: exit code `0`, build
  release, deploy e smoke test concluídos (ver ressalva do teste filtrado abaixo);
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`;
- `cargo fmt --all -- --check`: acusa somente drift preexistente do rustfmt 1.98
  (trechos novos formatados); `cargo check --workspace` e
  `git diff --check`: exit `0`;
- 1087 passed / 0 failed / 1 ignored em 87 suítes (1 teste preexistente quebrado
  filtrado via `--skip`: `tui_bridge ordinary_tui_second_turn_sends_prior_user_and_assistant`,
  cuja expectativa `Explicitly invoked skill` não tem implementação em `crates/`),
  incluindo proptest, fault injection,
  golden matrix via TestBackend e benchmark long-session com gate de budget;
- fixture offline localhost exercita o turno completo pela API TUI, sem chamada
  a provider live;
- o smoke ConPTY do binário implantado foi executado, mas este host emitiu só o
  probe `ESC[6n`, sem frame; a matriz física completa de terminal, IME, mouse e
  clipboard permanece não observada.

Deploy atual: `target\release\slim.exe` e `C:\Users\User\bin\Slim.exe` têm
18.706.944 bytes e SHA-256 idêntico
`3586F8F4B080C0E540073B404742254F3D56513C51D0EA90281C9864A3D62A76`;
`slim --version` retorna `slim 0.1.0` com exit `0`.

## Status atual de integraÃ§Ã£o

| Ãrea | Estado atual |
|---|---|
| Headless/provider/tools/auth/compaction/usage/artifacts/anti-loop | `Slim --headless` usa caminhos integrados e exercitados offline; OpenAI-compatible/Anthropic SSE, read/list/search/write/patch/shell e filtragem de capabilities funcionam. |
| TUI | `Slim` abre a interface normal mesmo deslogado. `/login` abre seletor OAuth nativo para Claude Pro/Max ou ChatGPT Plus/Pro; Anthropic Messages e Codex Responses usam o mesmo agent loop/tools Slim. Reducer Ãºnico normativo (`Action â†’ reduce â†’ Effect`), scrollback virtualizado e navegÃ¡vel (pin/live-edge/unseen), paleta estratificada Â§21.3, ActivityRail por fase/tempo, SessionRail conversacional adaptativa com contexto único e footer Grok-style responsivo (box alinhado + atalhos/metrics), tool blocks tipados agregados, lanes bounded com coalescer no runtime real, command palette Ctrl+P, markdown-light, motion bÃ¡sico com reduced motion e welcome estática com nome, conexão e próxima ação. Fixtures localhost provam OAuth/callback/store, streaming, tool round-trip e cancelamento. PTY E2E fÃ­sico pendente de console real (teste `#[ignore]`). |
| Cache/HTTP | Replay local de respostas (`ProviderCache`) desligado no produto; transporte HTTP compartilhado reaproveita conexões, enquanto adapters e autenticação continuam isolados por request. |
| SessÃµes | Writer/recovery/branch e `--session` escrevem JSONL novo; `--resume`/recovery explÃ­citos existem headless e TUI; UX geral de seleÃ§Ã£o/fork continua limitada. |
| Skills, MCP, subagentes | Tool `skill` lazy (`list` / `name`); pasta `%USERPROFILE%/.slim/skills` (cwd `.slim/skills` vence). Sem injeção de catálogo. Sem MCP/child. |
| Todo/Plan/Goal | Tool `todo` no loop Auto; `TodoChanged` abre o dock TUI. Plan/Goal sem UI; Plan headless continua abortando (N4). |
| Release | DeterminÃ­stico e hash-valid; empacota este checkpoint parcial, nÃ£o uma v1 completa. |

O restante desta pÃ¡gina descreve contratos realmente integrados. Para o plano de
integraÃ§Ã£o pendente, consulte o [Ã­ndice canÃ´nico](Documenta%C3%A7%C3%B5es%20-%20Projeto/README.md)
e o [plano](Documenta%C3%A7%C3%B5es%20-%20Projeto/PLANO-IMPLEMENTACAO.md). A fila
curta do ciclo atual (agente completo, harness leve) estÃ¡ em
[prÃ³ximas etapas](Documenta%C3%A7%C3%B5es%20-%20Projeto/PROXIMAS-ETAPAS-AGENTE.md).

## Contratos implementados no headless

No Windows, `auth.json` Ã© somente leitura e fail-closed. O arquivo Ã© aberto por
handle `windows-sys` e recebe DACL protegida com allowlist exata do owner atual,
usuÃ¡rio atual, `SYSTEM` e `Administrators`; o teste tambÃ©m cobre caminho Unicode.
Symlink, diretÃ³rio, reparse point, ACL divergente, JSON/schema invÃ¡lido ou
versÃ£o diferente falham. No headless, arquivo ausente nÃ£o Ã© criado. A TUI pode
escrever credenciais OAuth tipadas e a chave OpenCode Go no mesmo arquivo por
temp exclusivo, lock, ACL e replace atÃ´mico, preservando providers irmãos. NÃ£o hÃ¡ detecÃ§Ã£o mÃ¡gica de segredos desconhecidos.

O cliente HTTP nÃ£o segue redirects. O cache Ã© bounded, em memÃ³ria e por
processo, e estÃ¡ ativo no caminho normal. O transporte Reqwest Ã© compartilhado
para reaproveitar conexÃµes; seu namespace inclui provider, identidade do endpoint e modelo exato,
alÃ©m de mensagens/tools e conteÃºdo multimodal canonicalizados. Headers e chaves
ficam fora da chave. Respostas com tool call nunca sÃ£o cacheadas. SSE exige
evento terminal; bytes/eventos apÃ³s a terminaÃ§Ã£o ou stream sem terminaÃ§Ã£o sÃ£o
rejeitados. Os adapters preservam, quando fornecidos, os IDs/index/name de tool
deltas OpenAI e os IDs de `content_block` Anthropic; somente chamadas legadas
sem ID recebem identificador interno. JSON de tool malformado ou incompleto Ã©
rejeitado.

O runtime aplica budgets separados read-only vs mutating por run (`max_read_tool_calls` default 96, `max_mutating_tool_calls` default 32; env `SLIM_MAX_READ_TOOL_CALLS` / `SLIM_MAX_MUTATING_TOOL_CALLS` e chaves `slim.toml`); esgotamento → `tool_limit` (exit 22). O teto de turns por run é 128 (`SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024); esgotamento → `turn_limit` (exit 12) com mensagem `Turn limit reached (N/N)`. A tool `search` respeita `.gitignore`, ignora árvores de build, cap default 200 hits e paginação `offset`/`max_hits`. Emite `ToolStarted`, mantém pares assistant/tool e o prompt raiz através da
compactação no mesmo adapter/modelo. O threshold soft prepara o checkpoint em
background; o hard, `/compact` e uma única recuperação de overflow aplicam o
resumo somente após validação. A seleção preserva grupos assistant/tool, o
checkpoint durável usa fingerprint do prefixo, e usage/duração do resumo são
contabilizados mesmo quando uma preparação inválida é descartada. Cada valor de API key fornecido Ã© redigido exatamente antes
de tool output, follow-up ao provider, renderizaÃ§Ã£o e sessÃ£o; isso nÃ£o promete
encontrar qualquer segredo arbitrÃ¡rio que nÃ£o tenha sido registrado.

`--image PATH` aceita repetidamente PNG, JPEG/JPG, GIF e WebP; cada entrada deve
ser arquivo regular nÃ£o-symlink, nÃ£o vazio e ter no mÃ¡ximo 20 MiB. O conteÃºdo Ã©
enviado como base64 estrito, sem I/O remoto. `SLIM_CONTEXT_WINDOW_TOKENS` e
`SLIM_MAX_OUTPUT_TOKENS` aceitam inteiros positivos e controlam janela/reserva
de contexto e, quando suportado pelo wire, o teto de saÃ­da do provider (padrÃ£o:
4096 tokens). No contrato Codex subscription, o valor permanece reserva local e
`max_output_tokens` nÃ£o Ã© serializado. O resultado de tool tambÃ©m tem cap
bounded de 64 KiB, com artifact handle quando
aplicÃ¡vel.

No headless, text e JSONL expÃµem `stop`: `provider_completed` (exit `0`),
`turn_limit` e `repeated_failed_tool` (exit `12`) ou `tool_limit` (exit `22`).
O provider/model/IDs informados sÃ£o preservados; nÃ£o hÃ¡ troca silenciosa de
provider.

`--verbose` (somente com saída text) acrescenta a timeline humana de tools e o
Usage Ledger v2: input novo, cache write/read, reasoning, cache hit ratio,
latências, tentativas, compactação, ausência de progresso, economia de evidência
duplicada, erro da estimativa e custo por conclusão validada. Argumentos e
outputs não são exibidos; o modo padrão permanece answer-first e
`--verbose --jsonl` é rejeitado.

O JSONL de provider usa `version: 2` e inclui `usage` por requisição, totais da
execução, `costs`, `validation_source` e o qualificador
`compaction_tokens_saved_estimated`, preservando os campos agregados antigos como
projeção. Sem juiz externo, uma resposta final normal, sozinha, não é apresentada
como conclusão validada: o runtime também exige `ValidationGreen`, ausência de
erro terminal e, quando houve mutação, validação posterior à última mutação sem
modificação subsequente. Os preços continuam locais:
`SLIM_INPUT_COST_MICROS_PER_MILLION` e
`SLIM_OUTPUT_COST_MICROS_PER_MILLION`; cache pode ter taxas próprias em
`SLIM_CACHE_WRITE_COST_MICROS_PER_MILLION` e
`SLIM_CACHE_READ_COST_MICROS_PER_MILLION`. Se houver cache sem a taxa
correspondente, o custo exato permanece desconhecido em vez de usar a taxa de
input comum.

A estimativa inicial continua sem tokenizer específico (3,5 caracteres/token),
mas requests somente de texto, completos e não servidos pelo response cache a
calibram em memória por provider/modelo com EWMA de caracteres realmente
serializados por token total de input observado. Requests multimodais são
excluídos; cache lido/escrito permanece separado no ledger para não ser vendido
como economia comportamental do agente.

## Comece aqui

O Ã­ndice canÃ´nico estÃ¡ em
[DocumentaÃ§Ãµes - Projeto/README.md](Documenta%C3%A7%C3%B5es%20-%20Projeto/README.md).

As decisÃµes normativas estÃ£o em
[DECISOES-GRILL-PRE-IMPLEMENTACAO.md](Documenta%C3%A7%C3%B5es%20-%20Projeto/DECISOES-GRILL-PRE-IMPLEMENTACAO.md)
e o plano/evidÃªncia final em
[PLANO-IMPLEMENTACAO.md](Documenta%C3%A7%C3%B5es%20-%20Projeto/PLANO-IMPLEMENTACAO.md) e
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
`/login`, preservando entradas API-key existentes. O provider ativo salvo é
restaurado no próximo startup; variáveis de ambiente continuam tendo
precedência e evitam a leitura de um auth file inferior. Access/refresh/code nunca
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
