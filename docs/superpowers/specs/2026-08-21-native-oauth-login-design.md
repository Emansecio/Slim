# Slim — TUI deslogada e `/login` OAuth nativo

**Data:** 2026-08-21  
**Status:** design aprovado em direção; aguardando revisão deste documento  
**Referências:** `can1357/oh-my-pi`, `earendil-works/pi`, `Emansecio/Pitest` (MIT)

## Objetivo

`Slim` sempre abre a interface normal: histórico vazio ou recuperado, status e composer. Autenticação não é pré-condição para entrar em fullscreen.

Ao digitar `/login`, o composer abre um seletor dentro da própria TUI:

1. **Anthropic — Claude Pro/Max**;
2. **OpenAI Codex — ChatGPT Plus/Pro**.

O Slim executa OAuth nativo com PKCE, abre o navegador, recebe callback loopback, armazena e renova tokens com segurança e usa as APIs de assinatura diretamente. Nenhum executável `claude`, `codex`, Node ou Pi será dependência de runtime.

## Experiência da TUI

### Startup

`Slim` entra imediatamente na TUI mesmo quando não existe credencial. A superfície continua sendo a interface normal; não existe wizard ou tela de login inicial.

Estado deslogado aparece somente na linha operacional, por exemplo:

```text
SLIM Auto · signed out · type /login
```

Se usuário enviar prompt normal sem provider autenticado, draft é preservado e uma notificação aparece:

```text
No provider connected. Use /login.
```

### Comando `/login`

Composer reconhece `/login` como comando local e não o envia ao modelo. Abre overlay central:

```text
┌ Connect provider ──────────────────────┐
│ › Anthropic — Claude Pro/Max           │
│   OpenAI Codex — ChatGPT Plus/Pro      │
│                                        │
│ Enter connect · Esc cancel             │
└────────────────────────────────────────┘
```

- `↑`/`↓`: seleção;
- `Enter`: inicia OAuth;
- `Esc`: fecha overlay sem alterar sessão;
- durante OAuth, `Esc` cancela callback/polling;
- sucesso fecha overlay, ativa provider e atualiza status;
- falha fica visível como erro recuperável; TUI permanece aberta.

Também serão aceitos `/login anthropic` e `/login codex` para acesso direto e testes determinísticos.

### Logout

`/logout` remove credencial OAuth do provider ativo, limpa tokens da memória e retorna estado para `signed out`. Não remove variáveis de ambiente nem credenciais API-key criadas externamente.

## Arquitetura

### 1. Estado de startup sem credencial

`prepare_tui` deixa de construir obrigatoriamente um `ProviderRequest`. Ele produz configuração de sessão com:

- modo, modelo/endpoint opcionais e anexos;
- provider ativo opcional;
- auth resolvida opcional;
- prompt inicial opcional.

Worker TUI mantém `Option<ProviderSession>`. Somente um prompt autenticado cria execução real.

### 2. Comandos e eventos tipados

Novos comandos de UI:

```rust
UiCommand::StartLogin(OAuthProvider)
UiCommand::CancelLogin
UiCommand::Logout
```

Novos eventos:

```rust
UiEvent::AuthStateChanged { provider, state }
UiEvent::LoginProgress { message }
UiEvent::LoginUrl { url, user_code }
```

`slim-tui` controla overlay, seleção e rendering. `slim-cli` controla OAuth, filesystem, browser e construção do provider. Tokens nunca entram em `UiEvent`, blocks, session JSONL ou mensagens de erro.

### 3. Módulo OAuth

Novo módulo `slim-cli::oauth`, separado por responsabilidades:

- `pkce`: verifier aleatório, challenge SHA-256 e base64url sem padding;
- `callback`: listener exclusivamente em `127.0.0.1`, valida path e `state`, timeout e cancelamento;
- `anthropic`: authorization, exchange e refresh;
- `codex`: browser authorization, device-code fallback, exchange e refresh;
- `store`: leitura/escrita atômica e ACL Windows;
- `browser`: `ShellExecuteW` direto, sem `cmd /C` e sem interpolação de shell.

Interfaces de transporte, browser, relógio, aleatoriedade e store serão injetáveis nos testes. Produção usa implementações reais; testes usam fixtures localhost e memória.

## Protocolos

### Anthropic Claude Pro/Max

Fluxo alinhado ao `oh-my-pi`/Pi:

- authorization: `https://claude.ai/oauth/authorize`;
- token: `https://api.anthropic.com/v1/oauth/token`;
- PKCE S256;
- callback: `http://localhost:54545/callback`, listener em `127.0.0.1`;
- scopes: `org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload`;
- refresh com margem de cinco minutos;
- requests com `Authorization: Bearer`, betas `claude-code-20250219` e `oauth-2025-04-20`, `x-app: cli` e identidade Claude Code compatível.

O adapter Anthropic existente ganha auth tipada (`ApiKey` ou `OAuth`) para impedir mistura de `x-api-key` com Bearer.

### OpenAI Codex ChatGPT Plus/Pro

Fluxo alinhado ao `oh-my-pi`/Pi/Codex:

- authorization: `https://auth.openai.com/oauth/authorize`;
- token: `https://auth.openai.com/oauth/token`;
- callback fixo: `http://localhost:1455/auth/callback`;
- PKCE S256 e state separado;
- device fallback em `https://auth.openai.com/codex/device` quando porta 1455 estiver indisponível;
- account ID extraído do claim JWT `https://api.openai.com/auth`;
- refresh token rotativo;
- inference SSE: `https://chatgpt.com/backend-api/codex/responses`;
- headers `Authorization`, `chatgpt-account-id`, `OpenAI-Beta: responses=experimental`, `originator` e user-agent Slim;
- body Responses com `store:false`, `stream:true`, instructions, input, tools e `parallel_tool_calls:true`.

Novo `OpenAiCodexAdapter` normaliza text delta, reasoning delta, function calls, tool outputs, usage e terminal stop para os mesmos `ProviderEvent` já usados pelo runtime Slim. Primeira versão usa SSE; WebSocket fica fora de escopo.

## Credenciais e segurança

Arquivo canônico permanece `%USERPROFILE%\.slim\auth.json`.

Schema continua versionado e passa a aceitar credenciais tipadas por provider:

```json
{
  "version": 1,
  "active_provider": "openai-codex",
  "providers": {
    "anthropic": {
      "oauth": {
        "access": "...",
        "refresh": "...",
        "expires": 0
      }
    },
    "openai-codex": {
      "oauth": {
        "access": "...",
        "refresh": "...",
        "expires": 0,
        "account_id": "..."
      }
    }
  }
}
```

Regras:

- compatibilidade com entradas API-key atuais;
- criação/escrita temporária + flush + rename atômico;
- ACL Windows restrita ao usuário antes de considerar escrita bem-sucedida;
- recusa symlink, diretório e schema desconhecido;
- nenhum `Debug`, log, panic, event ou erro inclui access/refresh/code;
- refresh sincronizado para impedir corrida e rotação perdida;
- tokens expirados são renovados antes da request;
- `invalid_grant` remove sessão inválida da memória e pede novo `/login`, sem apagar arquivo silenciosamente;
- callback valida `state` antes do exchange;
- listener aceita somente loopback e path esperado;
- respostas HTTP de erro são truncadas e redigidas antes de chegar à UI;
- códigos OAuth colados pelo usuário nunca entram no histórico.

## Provider e seleção

Ordem na TUI:

1. provider OAuth ativo salvo;
2. provider indicado por `--provider` ou `SLIM_PROVIDER`, se tiver auth válida;
3. API key existente para compatibilidade;
4. signed out.

`/login` bem-sucedido torna provider selecionado o ativo. Modelo padrão será catalogado por provider e poderá ser sobrescrito por `--model`/`SLIM_MODEL`.

Headless não abre browser implicitamente. `Slim --headless` sem credencial mantém erro explícito; login interativo ocorre somente na TUI.

## Erros e cancelamento

- porta callback ocupada: Anthropic mostra instrução recuperável; Codex oferece device flow;
- navegador não abriu: URL continua visível e copiável;
- state incorreto: callback rejeitado, login continua aguardando callback válido até timeout;
- timeout: overlay retorna ao seletor;
- refresh falhou: prompt não é enviado, draft permanece;
- `Esc`/shutdown aborta request OAuth e fecha listener/polling;
- OAuth ativo impede segundo login concorrente;
- cancelamento de inference existente permanece independente do cancelamento de login.

## Testes

### Unitários

- PKCE/base64url e state;
- parsing de callback e rejeição de state/path;
- parsing de JWT Codex sem validar ou persistir payload integral;
- cálculo de expiração e refresh skew;
- schema backward-compatible e redacted Debug;
- selector `/login`, navegação, cancelamento e draft preservation;
- adapters Anthropic API-key/OAuth e Codex Responses.

### Integração offline

- TUI abre sem env/auth file;
- prompt deslogado não sai para rede e preserva draft;
- `/login` abre seletor normal dentro da TUI;
- fixtures localhost simulam authorize callback, exchange, refresh e device polling;
- browser launcher falso captura URL sem executar processo;
- login salva ACL e seleciona provider;
- tokens nunca aparecem em events, session, stdout/stderr ou snapshots;
- Codex request contém account ID e wire Responses correto;
- Anthropic OAuth usa Bearer/beta e não `x-api-key`;
- refresh rotativo é persistido atomicamente;
- cancelamento fecha listener/task;
- headless/API-key e TUI provider atual continuam verdes.

### Gates

- `cargo fmt --all -- --check`;
- `cargo clippy --workspace --all-targets -- -D warnings`;
- `cargo test --workspace`;
- `cargo build --workspace --release`;
- release determinística e checksums;
- revisão de segurança read-only focada em OAuth/store/callback/browser.

## Fora de escopo

- providers além de Anthropic e OpenAI Codex;
- login OAuth automático em headless;
- WebSocket Codex;
- importação de tokens do Claude Code, Codex ou Pi;
- dependência de executáveis externos;
- garantia comercial sobre como Anthropic/OpenAI contabilizam limites do plano; Slim usará protocolos de assinatura das referências, mas política de cobrança é controlada pelos providers.
