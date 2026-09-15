# Slim Ask Question — Design

**Status:** aprovado para especificação em 2026-08-24

**Objetivo:** permitir que o modelo pause uma execução TUI, faça uma pergunta tipada, ofereça sugestões selecionáveis e receba a resposta como resultado da mesma tool call.

## 1. Estado atual confirmado

O Slim já possui `InputRequired`, `ApprovalRequired`, `InteractionRequestId`, blocos inline e comandos de resposta. Esse contrato é defensivo, mas não é produtivo durante uma execução: o catálogo do modelo não anuncia uma tool de pergunta e o loop ativo da CLI rejeita `AnswerInput`, `Approve` e `Reject` como interações sem rota.

A feature deve completar esse seam sem converter perguntas em aprovações e sem reiniciar artificialmente o turno do provider.

## 2. Escopo

### Incluído

- tool `ask_question` disponível em runs TUI ativos Auto e ReadOnly quando houver uma rota interativa;
- uma pergunta por tool call;
- pergunta aberta, sem sugestões, ou pergunta com 2 a 5 sugestões;
- cada sugestão possui `label` e `description` curta;
- alternativa sintética `Outro...`, sempre disponível quando houver sugestões;
- navegação por `Up`/`Down`, seleção direta por teclas `1` a `5` e confirmação por `Enter`;
- resposta livre pelo composer para perguntas abertas ou após escolher `Outro...`;
- espera assíncrona, cancelável e correlacionada pelo ID da tool call;
- retorno estruturado ao modelo na mesma sequência de mensagens da tool call;
- eventos persistíveis, replay idempotente e tratamento explícito de resposta obsoleta.

### Não incluído

- formulário com várias perguntas na mesma call;
- seleção múltipla;
- perguntas interativas em modo headless;
- alteração do short-circuit normativo do modo Plan, que não inicia provider;
- perguntas em resume durável enquanto esse caminho mantiver `max_tool_calls=0` por ausência de persistência pré-efeito;
- inferência de perguntas a partir do texto normal do assistente;
- uso da resposta como autorização para mutações;
- provider real nos testes.

## 3. Contrato da tool

Nome: `ask_question`.

Payload lógico:

```json
{
  "question": "Qual abordagem devo usar?",
  "options": [
    {"label": "Incremental", "description": "Menor mudança e risco."},
    {"label": "Reescrita", "description": "Maior escopo e prazo."}
  ]
}
```

`options` é opcional. Os estados válidos são zero opções ou de duas a cinco. Uma opção isolada é inválida porque não representa uma escolha.

Limites:

- `question`: texto não vazio, uma linha, até 1.024 caracteres;
- `label`: texto não vazio, uma linha, até 80 caracteres;
- `description`: uma linha, até 256 caracteres;
- labels únicas após trim e comparação ASCII case-insensitive;
- resposta livre: texto não vazio após trim, até 16 KiB UTF-8;
- caracteres de controle, exceto o conteúdo normal do composer na resposta livre, são rejeitados.

Resultado devolvido ao modelo:

```json
{"answer":"Incremental","source":"option","option_index":0}
```

ou:

```json
{"answer":"Minha alternativa","source":"custom"}
```

O índice é zero-based e só existe para respostas originadas das sugestões.

## 4. Arquitetura

### 4.1 Core

Um módulo de interação define os tipos validados de pergunta, opção, resposta e um roteador assíncrono. O roteador registra exatamente uma espera por `InteractionRequestId`, entrega respostas somente ao ID correspondente e rejeita IDs duplicados ou obsoletos.

`Runtime` recebe a rota como capacidade opcional. Quando ausente, `ask_question` não entra no catálogo enviado ao provider. Quando presente, a definição é adicionada ao catálogo nos modos que efetivamente iniciam o provider; perguntar não lê nem altera o workspace. O short-circuit de Plan e o bloqueio de tools no resume durável permanecem inalterados.

A execução da call preserva o lifecycle causal:

1. `ToolStarted`;
2. `QuestionRequired` com o mesmo call ID;
3. espera por resposta ou cancelamento;
4. `InteractionAcknowledged`;
5. `ToolOutput` com o JSON estruturado;
6. `ToolFinished`.

O resultado é anexado como `ProviderMessage::tool` e o agent loop continua normalmente. Em batches seriais, calls posteriores só começam após a resposta.

Cancelamento remove a espera registrada, fecha o lifecycle da tool como falha/cancelada e impede que uma resposta tardia seja aceita.

### 4.2 CLI/TUI bridge

`start_active_run` cria o par rota/responder. A metade aguardada pelo runtime segue em `ProviderRunOptions`; a metade de resposta permanece em `ActiveRun`.

Eventos core continuam recebendo o namespace do run antes de chegar à TUI. Ao receber `AnswerQuestion`, a CLI valida e remove somente o prefixo do run ativo antes de encaminhar a resposta ao roteador core. IDs de outro run ou já concluídos recebem `InteractionAcknowledged` rejeitado e não acordam a execução.

`CancelRun` permanece funcional enquanto a pergunta está aberta.

### 4.3 Estado e reducer TUI

`InteractionRequestKind` ganha uma variante específica `Question`, preservando `Input` e `Approval` para compatibilidade. Seu estado mutável contém a opção selecionada e se o composer está no modo de resposta personalizada; esses campos não participam da identidade idempotente da request.

Com sugestões:

- a primeira opção inicia selecionada;
- `Up`/`Down` percorrem as opções e `Outro...`, com wrap;
- `1` a `5` selecionam a sugestão correspondente sem enviar;
- `Enter` sobre uma sugestão envia `AnswerQuestion` estruturado;
- `Enter` sobre `Outro...` ativa o composer; um segundo `Enter` só envia conteúdo não vazio.

Sem sugestões, o composer aceita texto imediatamente. Enquanto a resposta aguarda acknowledgement, navegação, edição, paste e reenvio ficam bloqueados.

### 4.4 Renderização

A pergunta pendente ocupa um painel dockado acima do composer, com a pergunta como corpo e as alternativas em lista vertical. O bloco inline no transcript permanece o registro compacto.

```text
  ╭ Question ──────────────── ↑↓ Enter ╮
  │ Qual abordagem devo usar?          │
  │                                    │
  │ [x] Incremental                    │
  │     Menor mudança e risco.         │
  │ [ ] Reescrita                      │
  │     Maior escopo e prazo.          │
  │ [ ] Outro...                       │
  ╰────────────────────────────────────╯
```

O painel é um cartão contido (máx. 72 colunas, inset 2), não full-bleed. Em terminais estreitos, a prosa quebra no limite de palavra e a descrição cai sob a própria alternativa; a seleção é o checkbox `[x]`, sem barra de highlight. `Up`/`Down`, `1`–`5` e `Enter` continuam válidos; o título só mostra `↑↓ Enter` ou `answer · Enter`, nunca Y/N de aprovação. Após confirmação, o painel some e o bloco mostra a resposta escolhida e o estado final, sem simular sucesso antes do acknowledgement do runtime.

## 5. Erros e invariantes

- payload inválido termina a tool com erro visível e não abre pergunta;
- somente uma resposta vence; duplicatas e respostas tardias são rejeitadas;
- uma request repetida com o mesmo ID e conteúdo é idempotente;
- mesmo ID com conteúdo diferente sinaliza resync, seguindo o contrato atual;
- ausência de rota nunca bloqueia: a tool não é anunciada e uma call inesperada falha explicitamente;
- nenhuma resposta de pergunta concede aprovação ou eleva permissões;
- pergunta, opções e resposta passam pelos limites e sanitização antes de cruzar core, TUI e provider.

## 6. Verificação

O trabalho seguirá slices TDD verticais:

1. schema/validação e catálogo condicional;
2. roteador assíncrono, resposta correlacionada e cancelamento;
3. agent loop completo `ask_question -> resposta -> continuação` com provider fake;
4. roteamento do `ActiveRun` na CLI;
5. reducer/render com setas, números, `Outro...` e resposta aberta;
6. fixture offline TUI e regressão integral.

Cada slice deve demonstrar RED antes do GREEN, passar revisão local antes do próximo e manter os contratos de lifecycle, backpressure e cancelamento existentes.

O gate final exige `cargo test --workspace`, Clippy proporcional, `cargo fmt --all -- --check`, documentação viva, `refresh-slim.ps1 -Test` com `OK:` e prova de identidade entre o release recém-gerado e `C:\Users\User\bin\Slim.exe`.
