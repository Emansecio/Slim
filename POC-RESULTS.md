# Slim — evidência de POCs, gates e release

> **Status de implementação:** evidência de componentes e do caminho headless
> em um checkpoint de integração parcial. Este arquivo não é prova de v1
> completa. Atualizado em 2026-08-21.

> A TUI do binário abre normalmente sem credencial; `/login` oferece OAuth nativo
> Anthropic Claude Pro/Max e OpenAI Codex ChatGPT Plus/Pro. Composer, auth/config,
> provider SSE, agent loop, tools, usage e cancelamento usam caminhos reais. Cache normal, resume/branch de sessão,
> Skills, MCP, subagentes e Todo/Plan/Goal ainda têm implementação ou contratos
> testados sem wiring completo no caminho normal do produto.

## Gates observados

Os quatro comandos abaixo retornaram exit code `0` no workspace Windows
MSVC:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

O gate contém **159 testes em 40 seções de teste**. Há E2E offline para
Anthropic, OpenAI-compatible, Codex Responses, PKCE/callback OAuth, store com
ACL e CLI com `auth.json`; há também E2E do binário real usando provider
localhost. Nenhuma credencial real, rede externa ou provider live foi usado.

Os 159 testes/40 seções demonstram componentes, headless e a bridge TUI real
contra localhost; não demonstram a integração de todas as capabilities da v1. `--session` grava um
JSONL novo, mas não oferece resume, seleção de recovery ou branch/fork na CLI ou
TUI.

## Contratos comprovados

### Confiança, ACL e redaction

No Windows, o carregamento de `auth.json` usa handle nativo `windows-sys`,
`FILE_FLAG_OPEN_REPARSE_POINT` e leitura pelo mesmo handle. A DACL é protegida e
contém exatamente owner atual, usuário atual, `SYSTEM` e `Administrators`, cada
um com allowlist de acesso; há teste para caminho Unicode. Symlink, diretório,
reparse point, ACL diferente, leitura impossível ou schema/versão inválidos
falham fechando. Headless mantém arquivo ausente; `/login` TUI cria/atualiza
`auth.json` por temp exclusivo aleatório, ACL e replace atômico.

O valor exato de cada API key fornecido é registrado sem ser armazenado em
diagnóstico e redigido antes de tool output, follow-up ao provider,
renderização e persistência de sessão. Isso é redaction por valores conhecidos:
segredos arbitrários não registrados não são alegadamente detectados.

### Provider e multimodal

- Redirects HTTP estão desabilitados.
- Cache opcional é em memória e por processo. A implementação existe em
  `HttpProviderClient` e é testada, mas os construtores normais usam
  `HttpProviderClient::new`, portanto não está ativo no produto. Quando
  habilitado, o namespace inclui provider, identidade segura do endpoint,
  modelo exato e digest de mensagens/tools e
  conteúdo multimodal; headers, query/fragmento de endpoint e credenciais ficam
  fora da chave. Streams incompletos/falhos e qualquer resposta com tool call
  não entram no cache.
- SSE é estrito: o stream precisa de evento terminal; `[DONE]`/stop encerra a
  leitura e eventos posteriores ou ausência de terminação são rejeitados.
- Deltas de tool OpenAI preservam `index`, `id` e `name` quando fornecidos;
  Anthropic preserva `content_block` `index`/`id`/`name`. JSON malformado ou
  incompleto é rejeitado antes de executar a tool.
- Base64 multimodal exige forma canônica estrita. `--image` é repetível e aceita
  somente PNG/JPEG/JPG/GIF/WebP, arquivo regular não-symlink, não vazio e <=20
  MiB; não há busca de arquivo/URL remoto durante a normalização.
- `SLIM_CONTEXT_WINDOW_TOKENS` e `SLIM_MAX_OUTPUT_TOKENS` aceitam somente
  inteiros positivos. O teto padrão de saída é 4096 tokens; resultados de tool
  têm cap de 64 KiB, com artifact handle quando necessário.

### Runtime, contexto e headless

- Budgets separados read-only vs mutating por run (`max_read_tool_calls` / `max_mutating_tool_calls`); esgotamento → `tool_limit` (exit 22). `ToolStarted` precede execução; pares assistant/tool preservados na compactação.
- O provider/model informado é preservado. Compaction bounded usa o mesmo
  adapter/modelo, preserva o prompt raiz e pares assistant/tool completos, e
  persiste `Usage` tanto dos turnos quanto do resumo.
- Tool call repetida após falha é bloqueada; argumentos JSON malformados ou
  incompletos não chegam à execução.
- Text e JSONL expõem a parada e os códigos: `provider_completed` → `0`,
  `turn_limit` → `12`, `repeated_failed_tool` → `12`, `tool_limit` → `22`.

## Release determinístico

Com `target/release/slim.exe` compilado, o builder foi executado duas vezes com
Python 3.12 (`python3.exe`); o alias `python3` local do PATH retornou acesso
negado, mas a mesma entrada foi executada com o interpretador instalado. As
duas execuções produziram o mesmo ZIP. O manifesto e
`release/SHA256SUMS.txt` foram regenerados a partir dos bytes reais; a validação
`sha256sum -c release/SHA256SUMS.txt` retornou `OK` para EXE e ZIP.

O ZIP lista exatamente uma entrada, `slim.exe`, com timestamp fixo
`1980-01-01 00:00:00`. A varredura de nomes e bytes não encontrou markers de
segredo das fixtures. O builder usa somente a biblioteca padrão do Python,
resolve caminhos a partir do próprio arquivo e escreve apenas em `release/`;
nenhum artefato temporário literal fora desse diretório foi criado.

Saídas e hashes observados nesta execução:

```text
slim.exe --version stdout: slim 0.1.0
slim.exe --version exit: 0
slim.exe --help stdout:
Slim coding agent
Usage: Slim [TUI OPTIONS]
       Slim --headless [--plan|--read-only|--jsonl|--provider NAME|--model MODEL|--endpoint URL|--session PATH|--image PATH|--prompt TEXT]
slim.exe --help exit: 0
slim.exe --headless --unknown-option stderr: unknown option: --unknown-option
slim.exe --headless --unknown-option exit: 30
SHA-256 EXE: FD13F14173EB93F6279CC1426242E8E23F88C96D51F9B274E61D773586C8452E
SHA-256 ZIP: 96CE34B042B9702272F1590A116B36BDDC10ABF10C174F17AE83D18C7043AE9F
  (idêntico nas duas execuções)
```

## Limitações restantes

Não foi feita chamada a provider live. A evidência HTTP é de fixtures localhost
e a evidência de terminal é ConPTY/testkit local. Continua sem prova física uma
matriz completa de emuladores/terminais, lifecycle prolongado, IME, paste
multiline, mouse, clipboard, imagens e fallbacks. O release determinístico não
deve ser descrito como validação física ou produção universal.
