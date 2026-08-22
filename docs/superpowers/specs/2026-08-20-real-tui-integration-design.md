# Integração da TUI real do Slim

**Data:** 2026-08-20  
**Status:** aprovado por delegação explícita de autonomia  
**Milestone:** substituir `slim_tui::demo::run_demo()` por TUI conectada ao coding agent real

## Objetivo

`slim --tui`, configurado contra provider TCP localhost, deve aceitar prompt no composer, projetar streaming e lifecycle de tools enquanto acontecem, executar tools pelo runtime existente, mostrar resposta/usage e restaurar terminal em saída normal, erro ou cancelamento. Headless e TUI devem compartilhar construção e execução; TUI não executa domínio.

## Escopo

Incluído:

- parsing compartilhado de provider/model/endpoint/mode/session/image;
- auth existente por env/`auth.json`;
- runtime, provider adapters e tool registry existentes;
- bridge tipada `UiCommand`/`UiEvent`;
- prompt do composer;
- streaming real de texto/reasoning/lifecycle;
- tool call, tool output e tool completion;
- usage e estado working/idle;
- cancelamento de request provider em andamento;
- shutdown e restauração idempotente do terminal;
- fixture TCP localhost sem provider externo;
- primeira fatia RED já existente em `crates/slim-cli/tests/tui_runtime.rs`.

Fora deste milestone:

- ativar cache HTTP normal;
- resume/recovery/branch de sessão;
- Skills, MCP, subagentes e Todo/Plan/Goal end-to-end;
- refazer renderer M0–M3 já existente;
- provider live, OAuth, browser, terminal físico ou novo crate.

Esses itens permanecem nos milestones seguintes do plano canônico.

## Escolha arquitetural

Composition root fica em `slim-cli`.

- `slim-core`: provider, normalização, agent loop, tools, eventos e cancelamento por drop/abort do future.
- `slim-tui`: reducer, composer, terminal, render e loop de UI; recebe channels tipados, sem conhecer auth/provider.
- `slim-cli`: parse/config/auth, construção de adapter/runtime, worker assíncrono e ligação entre core e TUI.

Não criar interface/factory genérica para dois adapters. Branch explícito por `ProviderKind` continua menor e auditável.

## Fatias

### 1. Preservar RED atual

Implementar `run_provider_tui_turn()` como boundary de projeção testável. Função roda caminho provider/tool real e devolve `Vec<UiEvent>`. Adicionar `UiEvent::Usage` e projeção correspondente. Essa função prova prompt → tool → resultado → provider → final sem terminal.

Ela não será alegada como streaming ao vivo; serve como primeira fatia vertical e helper de contrato.

### 2. Streaming no core

Hoje `HttpProviderClient` entrega eventos incrementalmente, mas `Runtime::run_provider_messages()` usa `send_messages()` e só normaliza o `Vec` completo. Extrair normalizador stateful do código robusto já existente em `runtime/mod.rs` e alimentá-lo via `stream_messages()`.

Regras preservadas:

- IDs/index/name de tool fragments;
- JSON de tool completo antes de publicação/execução;
- evento terminal obrigatório;
- eventos após terminal rejeitados;
- redaction antes de publicar;
- sequência monotônica;
- tool limits antes de efeito.

`AppHandle` ganha observer opcional de `SessionEvent`. Persistência em memória continua autoridade; observer é fanout best-effort para apresentação. Receiver desconectado não invalida sessão nem provider run.

### 3. Bridge assíncrona

`slim-cli` cria worker Tokio dedicado e channels bounded/standard-library onde possível:

```text
TUI --UiCommand--> worker --provider/runtime/tools--> AppHandle
TUI <--UiEvent--- projector <--SessionEvent--------- AppHandle observer
```

Um run ativo por sessão TUI. `SendPrompt` durante run não inicia segundo mutator; será rejeitado visivelmente nesta fatia, preservando draft. `SetMode` altera capability surface do próximo run. `Shutdown` encerra worker. `CancelRun` aborta future provider ativo; drop do request fecha transporte e worker emite estado cancelado/idle.

Sem thread kill, sleep ou polling longo. Channels e `tokio::select!` coordenam command/run.

### 4. Loop fullscreen real

Substituir demo por `slim_tui::run_app(...)`:

- drenar `UiEvent`s antes de render;
- caracteres e paste atualizam composer;
- Enter envia payload não vazio;
- Ctrl+Enter/Shift+Enter inserem newline conforme decoder existente;
- Shift+Tab envia `SetMode`;
- Ctrl+C durante run envia `CancelRun`;
- Ctrl+C idle com draft vazio envia `Shutdown`;
- resize redesenha;
- frame imediato para input, lifecycle final e primeiro delta; demais eventos drenados em janela curta já suportada;
- `FullscreenBackend::shutdown()` continua caminho explícito; RAII cobre erro/panic.

TUI só traduz input em command e event em state. Provider, tools, auth e sessão não entram em `slim-tui`.

### 5. Parsing e composição compartilhados

Extrair parse atual de `run_cli()` para estrutura interna única. Headless mantém saída e exit codes existentes. `main.rs` deixa de interceptar `--tui` para chamar demo; passa opções parseadas ao composition root TUI.

Prioridades e defaults existentes permanecem:

- CLI > env;
- `SLIM_API_KEY` > provider-specific env > `auth.json`;
- mesmos providers/endpoints/modelos;
- mesmos limites de contexto/output;
- nenhuma chamada externa em testes.

## Estado e projeção

Adicionar somente estado necessário ao milestone:

- usage acumulado (`input_tokens`, `output_tokens`);
- erro/cancelamento visível;
- activity rows para tool start/finish;
- notification/tool output bounded;
- working inicia no envio/primeiro delta e termina em completion/error/cancel.

Não criar novo modelo visual paralelo. Reusar `AppState`, `UiEvent::from_core`, reducer e `ViewModel`.

## Erros

- Falha de parse/auth antes de entrar fullscreen: stderr + exit code existente.
- Falha provider após fullscreen: `FatalError`/notification, draft preservado quando send não inicia.
- Tool failure: lifecycle visível; agent loop decide continuação.
- Channel fechado: shutdown visível e restauração terminal.
- Cancelamento: não inventar `AssistantEnded`; emitir evento de cancelamento próprio e voltar idle.
- Restore failure: retornar erro após tentar todos passos idempotentes.

Secrets conhecidos continuam redigidos antes de event, render e sessão.

## Testes TDD

1. Manter RED observado em `tui_runtime.rs`; fazê-lo passar sem alterar expectativa central.
2. Teste core com fixture controlada por channels: primeiro SSE delta deve chegar ao observer antes de servidor liberar evento terminal. Sem `sleep`.
3. Teste tool fragmentado preserva ID e só executa após JSON completo.
4. Teste bridge: `SendPrompt` produz user/tool/final/usage em ordem.
5. Teste cancel: fixture aceita request e aguarda fechamento; `CancelRun` encerra future e emite cancelled/idle, sem processo órfão.
6. Teste reducer/composer: submit limpa draft somente após command aceito; falha preserva draft.
7. Teste CLI: `--tui` aceita provider/model/endpoint/session/image e não passa pelo demo.
8. Rodar gates completos com toolchain MSVC explícito.

## Critério de pronto

- `run_demo()` não é chamado pelo binário;
- fixture localhost recebe prompt, emite tool call, recebe tool result e emite final;
- TUI recebe primeiro delta antes do fim do SSE;
- tool lifecycle, resposta e usage aparecem no `AppState`/frame;
- cancel fecha provider run e UI volta idle;
- terminal restore continua coberto;
- headless mantém contratos e testes;
- fmt, Clippy `-D warnings`, workspace tests e release build passam;
- docs/status e release determinístico são atualizados após gates.
