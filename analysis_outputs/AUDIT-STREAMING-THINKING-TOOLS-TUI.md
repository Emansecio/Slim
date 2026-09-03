# Auditoria de streaming, thinking e tools na TUI do Slim

- **Data:** 2026-08-24
- **Escopo:** auditoria crítica, somente leitura, do fluxo `slim-core → slim-cli → slim-tui`
- **Binário observado:** `C:\Users\User\bin\Slim.exe`
- **Resultado:** nenhum código, teste, configuração, documentação normativa ou binário foi alterado.

## 1. Veredito executivo

A TUI atual entrega um happy path visualmente limpo: reasoning, tool em execução, tool concluída, nova reasoning, resposta final, falha e cancelamento têm glyphs e hierarquia legíveis. Buffers VT/ConPTY observados nesta sessão mostraram isso em `120×30`, `80×24`, `60×16`, `40×10` e `32×10`; os recortes persistidos abaixo são evidência da sessão, não um gate reproduzível versionado.

O problema central não é acabamento; é **fidelidade causal e inspecionabilidade**. A superfície mostra o nome da tool e uma primeira linha, mas descarta call ID e argumentos, não oferece acesso ao output integral e não permite expandir reasoning pela UI atual. Além disso, após uma tool, usa `Responding` antes de chegar qualquer byte do provider. A spec admite `Responding` como fase, mas não define essa fronteira; portanto isso é uma ambiguidade semântica comprovada, não uma violação normativa fechada.

Não há P0. Os oito achados se distribuem em **3 P1, 4 P2 e 1 P3**. Os quatro que dominam a experiência são:

1. tool calls não são auditáveis pela interface;
2. o lifecycle de reasoning não possui início/fim tipados;
3. reasoning recolhido não é expansível pela interação atual, apesar do contrato prometer expansão;
4. como P2, a fase pós-tool chama de `Responding` um intervalo que também pode ser apenas espera pelo provider.

O fluxo não precisa de redesign amplo. A direção recomendada é preservar a linguagem visual atual e tornar cada transição **causal, identificável e progressivamente inspecionável**.

## 2. Escopo, método e prova visual

### 2.1 Fontes inspecionadas

Foram lidos nesta sessão:

- `RULES.md`, `AGENTS.md`;
- `Documentações - Projeto/DESIGN-SLIM-TUI.md`;
- `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md`;
- eventos e runtime de `slim-core`;
- bridge e lifecycle em `crates/slim-cli/src/tui.rs`;
- `api`, `app`, `block`, `reducer`, `render`, `runtime`, `layout`, `theme`, `view_model`, `markdown` e testes de `slim-tui`;
- goldens, testes de bridge/runtime, ConPTY ignorado e benchmark de long session.

Antes da única escrita autorizada, o worktree já continha **174 entradas** em `git status --short`; depois da criação deste relatório, passou a `175`. As 174 preexistentes foram tratadas como WIP do usuário e preservadas.

### 2.2 Identidade do binário

Comandos executados:

```powershell
Get-FileHash 'C:\Users\User\bin\Slim.exe' -Algorithm SHA256
Get-FileHash 'D:\Slim\target\release\slim.exe' -Algorithm SHA256
Get-Item 'C:\Users\User\bin\Slim.exe','D:\Slim\target\release\slim.exe'
& 'C:\Users\User\bin\Slim.exe' --version
```

Resultado verificado:

- PATH e `target\release` têm `8.723.968` bytes;
- ambos têm SHA-256 `03D408BBA1ACFFE6D31F0A9E657E45FADC144AC7864E73FBA3969436BE237328`;
- ambos têm timestamp `2026-08-24 03:36:27 -03`;
- `--version` retornou `slim 0.1.0`, exit `0`;
- o executável é posterior ao último timestamp de fonte inspecionado (`03:32:25`).

Isso elimina o risco prático de ter observado uma cópia antiga do PATH. Não é uma atestação de build reproduzível.

### 2.3 Fixture e captura real

O binário implantado foi iniciado por `pywinpty 3.0.2`/ConPTY, com dimensões definidas antes do spawn. Um servidor HTTP/SSE efêmero em `127.0.0.1` emitiu reasoning, texto, tool calls, falha e pausas controladas. Não houve provider comercial.

Invocação equivalente em todos os casos:

```powershell
$env:SLIM_API_KEY = 'fixture-key'
$env:SLIM_AUTH_FILE = "$env:TEMP\slim-audit-missing-auth.json" # inexistente
& 'C:\Users\User\bin\Slim.exe' --tui `
  --provider openai-compatible `
  --endpoint 'http://127.0.0.1:<porta-efemera>' `
  --model fixture-model `
  --prompt 'audit <cenario>'
```

Não foram usados `--session`, `--resume` ou `--recover`. O runner e os frames brutos existiram somente em memória; os recortes relevantes foram persistidos exclusivamente neste relatório, dentro da única escrita permitida. O output VT foi aplicado a uma grade de células e observado como **buffer textual do binário**, não como screenshot pixel a pixel do Windows Terminal nem como golden automatizado/versionado.

| Cenário | Dimensão | Estado capturado | Evidência observada |
|---|---:|---|---|
| reasoning → answer | `120×30` | reasoning ativo e final | `Thinking` durante stream; reasoning recolhido + bloco `Slim` no final |
| reasoning → tool → reasoning → answer | `80×24` | tool ativa e final | `Running shell`; dois blocos de thinking separados; tool `✓` |
| duas tools consecutivas | `60×16` | final | duas calls `shell` viraram `✓ shell ×2` |
| tool ativa | `40×10` | running | rail visível, label truncado para `Runni` |
| NO_COLOR + reduced motion | `32×10` | reasoning ativo | zero SGR de cor; ActivityRail oculta; footer `Working… ^C` |
| tool failure | `80×24` | final | `✕ read · failed · io error...` |
| tool cancel | `80×24` | terminal | `■ shell · cancelled` + `run cancelled` |
| espera do provider pós-tool | `80×24` | gap antes de qualquer delta | `✓ shell` + `Responding`, embora o segundo request ainda não tivesse emitido conteúdo |

Recortes literais dos buffers VT observados, removendo apenas rows vazias para caber no relatório:

```text
[120×30 · final]
D:\Slim                                      ctx ~3% · 1.2k/32k
  you
│ audit 120-reasoning-answer
  ◌ thinking · Primeiro verifico os eventos; depois comparo o contrato.
  Slim
│ Resposta final offline: fluxo concluído.
  Shift+Tab:mode │ Ctrl+C:exit │ Ctrl+P:commands

[80×24 · tool final]
  ◌ thinking · Vou executar uma ferramenta offline.
  ✓ shell · exit_code=Some(0) timed_out=false cancelled=false
  ◌ thinking · A ferramenta terminou; agora sintetizo.
  Slim
│ Resposta depois da ferramenta.

[60×16 · duas tools]
  you
│ audit 60-multiple-tools
  ✓ shell ×2
  Slim
│ Duas ferramentas concluídas.

[40×10 · running]
  you
│ audit 40-running
  ◌ shell
 ◒ Runni · 0s    ctx ~3%    Ctrl+C stop
 ^C          ctx ~3% · 1.1k/32k · ↑0 ↓0

[32×10 · NO_COLOR + reduced motion]
  you
│ audit 32-no-color-reduced
  ◌ thinking · Raciocínio em mod
 ›
 Working… ^C    ctx ~3% · ↑0 ↓0

[80×24 · failure]
  ✕ read · failed · io error: O sistema não pode encontrar o arquivo especificad
  Slim
│ Continuei após a falha da ferramenta.

[80×24 · cancel]
  ■ shell · cancelled
run cancelled

[80×24 · provider gap pós-tool]
  ✓ shell · exit_code=Some(0) timed_out=false cancelled=false
 ◐ Responding · 0s    ctx ~3%    Ctrl+C stop
```

Os tempos de sincronização do runner (`~3–5 s`) incluem boot e delays artificiais da fixture; **não são benchmark**.

## 3. Fluxo causal atual

```text
prompt
  │
  ├─ UiCommand::SendPrompt
  ├─ RunStarted ───────────────► working=true ─► ActivityRail: Working
  │
  ├─ ReasoningDelta* ──────────► ThinkingDelta ─► bloco Thinking collapsed
  │                                             ActivityRail: Thinking
  │        (não existe ThinkingStarted/ThinkingEnded)
  │
  ├─ AssistantTextDelta* ──────► sem terminal tail, fecha Thinking streaming
  │                              bloco Assistant ─► ActivityRail: Responding
  │
  └─ ProviderToolCall ─────────► descartado no bridge visual
           │
           ├─ ToolStarted ─────► bloco Tool(name) ─► Running <name>
           ├─ ToolOutput ──────► bridge normal: primeira linha, até 512 chars
           └─ ToolFinished ────► ✓/✕ ─► Responding somente se a tool
                                      │  era atual e a run segue ativa
                                      │
                                      ├─ espera do provider: ainda “Responding”
                                      └─ novo ReasoningDelta: novo bloco Thinking

AssistantEnded = fence contábil/fecha assistant, mas não encerra a run
RunCompleted / RunStopped / RunCancelled / RunFailed
  └─ terminaliza blocos streaming, remove ActivityRail e ajusta footer/toast/error
```

O fluxo visual é uma projeção, não autoridade — decisão correta. A falha é que a projeção perde dados causais necessários para explicar o próprio fluxo.

## 4. Matriz completa das 16 transições

Regra transversal da matriz: enquanto `working=true`, o footer mantém a ação de cancelamento; após um outcome, volta a `exit`. Com cor, Activity usa warning/text/muted; Thinking usa `palette.thinking`; Assistant usa accent/text; tool running, success, failure e cancel usam respectivamente warning/tool, success, error e warning/muted (`crates/slim-tui/src/runtime.rs:1034-1095`, `crates/slim-tui/src/runtime.rs:1212-1287`, `crates/slim-tui/src/runtime.rs:1326-1379`). `NO_COLOR` foi verificado dinamicamente no cenário `32×10` (zero SGR de cor, glyph/label preservados), não em cada um dos 16 estados. Reduced motion congela o spinner, substitui o caret por espaço sem reflow e mantém elapsed semântico de 1 s somente se visível (`crates/slim-tui/src/runtime.rs:1034-1048`, `crates/slim-tui/src/runtime.rs:1333-1379`, `crates/slim-tui/src/runtime.rs:738-772`). Em `32×10`, o efeito de spinner não ficou visível porque a ActivityRail foi ocultada. Quando a rail some por altura, o estado específico é substituído por `Working…`. As células abaixo registram as exceções e perdas próprias de cada transição.

| # | Transição | Evento/bridge | Estado no reducer | Transcript + ActivityRail + footer | Narrow / caps | Informação invisível |
|---:|---|---|---|---|---|---|
| 1 | Prompt enviado / espera inicial | CLI injeta prompt; `RunStarted` abre a run | user block; `working=true`; activity `None` (`crates/slim-tui/src/app.rs:537-553`) | prompt no transcript; rail `Working` em warning + elapsed muted; footer oferece cancel (`crates/slim-tui/src/runtime.rs:1034-1095`) | sem rail, footer normativo `Working… ^C` (`crates/slim-tui/src/runtime.rs:1640-1661`) | request HTTP, round/tentativa e transporte |
| 2 | Reasoning iniciado | primeiro core `ReasoningDelta` vira `ThinkingDelta` | cria Thinking streaming e `Collapsed`; activity `Thinking` (`crates/slim-tui/src/app.rs:709-731`) | `◌ thinking · <primeira linha>` em `palette.thinking`; rail `Thinking` (`crates/slim-tui/src/runtime.rs:1049-1059`, `crates/slim-tui/src/runtime.rs:1243-1250`) | rail pode sumir; NO_COLOR preserva glyph/label | não há start tipado, stream ID ou origem |
| 3 | Reasoning em streaming | deltas seguintes | concatena somente quando o **último** bloco é Thinking compatível; mantém fase (`crates/slim-tui/src/app.rs:709-731`) | a mesma row muda; elapsed continua | reduced motion congela spinner, sem remover texto (`crates/slim-tui/src/runtime.rs:1034-1082`) | quantidade/ordem de chunks e fronteiras do stream |
| 4 | Silêncio após `ThinkingDelta` / boundary ausente | **não há `ThinkingEnded`** (`crates/slim-core/src/events.rs:18-32`) | silêncio não fecha nada; `AssistantDelta` fecha Thinking apenas sem terminal tail; `ToolStarted` muda a fase, mas não fecha o bloco (`crates/slim-tui/src/app.rs:663-676`, `crates/slim-tui/src/app.rs:815-833`) | não há glyph de término; no silêncio a rail permanece `Thinking` | sem rail, fica apenas `Working…` | não é possível saber se reasoning terminou; faltam instante real do fim e boundary antes do próximo domínio |
| 5 | Primeira parte da resposta | primeiro `AssistantTextDelta` → `AssistantDelta` | sem terminal tail, fecha o Thinking streaming encontrado e entra em `Responding`; cria Assistant streaming (`crates/slim-tui/src/app.rs:663-690`) | `Slim` em accent + Markdown e caret em accent (`crates/slim-tui/src/runtime.rs:1231-1240`, `crates/slim-tui/src/runtime.rs:1326-1379`) | reduced motion reserva a célula, mas oculta o caret | stream ID e TTFT separado de bufferização |
| 6 | Resposta textual em streaming | `AssistantDelta` seguinte | concatena apenas no Assistant compatível; terminal tail preserva lifecycle terminal (`crates/slim-tui/src/app.rs:668-690`) | corpo Markdown cresce; caret apenas se streaming/visível | wrap/reflow acompanha largura; NO_COLOR remove dependência de cor | sequência dos chunks e cadência de frames |
| 7 | Tool call detectada pelo provider | `ProviderToolCall`/`ToolCall` | bridge retorna `None` (`crates/slim-tui/src/api.rs:456-460`) | nenhum sinal preliminar | igual | ID, argumentos, ordem e intervalo até o executor; a call só surge em `ToolStarted` |
| 8 | Ferramenta iniciada | executor emite `ToolStarted { name }` (`crates/slim-core/src/runtime/mod.rs:1074-1095`) | cria um bloco independente por start; se não há terminal tail, activity `RunningTool(name)` (`crates/slim-tui/src/app.rs:815-833`) | `◌ name` em warning/tool; rail `Running name` | `40×10` mostrou `Runni`; sem rail, `Working…` | call ID, argumentos, origem e cancelamento individual |
| 9 | Ferramenta produz output | `ToolOutput` → `ToolProgress` | no caminho core→TUI normal, limita à primeira linha/512 chars e procura a última Tool **streaming de mesmo nome** (`crates/slim-tui/src/api.rs:461-504`; `crates/slim-tui/src/app.rs:834-841`) | primeira linha; depois pode truncar por largura | sem mudança semântica; apenas menos células | output integral, linhas posteriores, cursor/página e ContentHandle; `UiEvent::ToolProgress` direto não impõe o limite de 512 |
| 10 | Ferramenta conclui, falha ou é cancelada | `ToolFinished(success)`; cancelamento da run | sucesso→`Complete`, falha→`Failed`; `RunCancelled` só converte blocos ainda streaming em `Cancelled` (`crates/slim-tui/src/app.rs:843-864`, `crates/slim-tui/src/app.rs:609-619`) | `✓` success, `✕ … failed`, `■ … cancelled`, com success/error/warning (`crates/slim-tui/src/runtime.rs:1264-1287`) | detalhe pode truncar; footer volta a exit no terminal | duração, status estruturado e cancelamento individual; se `ToolEnded(false)` chegar antes, o bloco já Failed não vira Cancelled |
| 11 | Múltiplas ferramentas | provider entrega vetor; core percorre `for call` serialmente (`crates/slim-core/src/runtime/mod.rs:789-830`) | cada `ToolStarted` cria bloco; progress/end correlacionam só por nome (`crates/slim-tui/src/app.rs:815-855`) | completas, consecutivas e homônimas agregam visualmente em `✓ name ×N` (`crates/slim-tui/src/render.rs:175-200`; `crates/slim-tui/src/runtime.rs:860-877`) | grupo economiza rows | IDs, args e outputs dos membros; o paralelismo do mapa de arquitetura-alvo (`Documentações - Projeto/RUST-CLI.md:3-4`, `Documentações - Projeto/RUST-CLI.md:257-260`) não ocorre hoje |
| 12 | Retorno da ferramenta ao provider | não há UiEvent de “awaiting provider” | somente se `ended && was_current && working && terminal_tail.is_none()`, `ToolEnded` entra em `Responding` (`crates/slim-tui/src/app.rs:843-863`) | buffer da sessão mostrou `✓ shell` + `Responding` durante o segundo request silencioso | sem rail, degrada normativamente para `Working…` | transporte, TTFT/retry e diferença entre espera e resposta ativa |
| 13 | Nova fase de reasoning após tool | novo `ReasoningDelta` | em run ativa e sem terminal tail, como o último bloco é Tool, cria novo Thinking e volta a `Thinking`; após terminal, cria lifecycle terminal sem reativar fase (`crates/slim-tui/src/app.rs:709-731`) | dois thinking blocks separados no buffer `80×24` | cada um recolhido em uma linha | vínculo explícito com round/tool anterior |
| 14 | Resposta final | `AssistantEnded` | conclui o último Assistant que ainda esteja streaming e projeta usage; sem tal bloco, não muda lifecycle; ainda não encerra `working` (`crates/slim-tui/src/app.rs:695-707`) | resposta fica sem caret quando o bloco foi concluído; rail só some no outcome seguinte | texto continua responsivo/wrapped | motivo final/stop e fronteira entre texto final e outcome |
| 15 | Run concluída, cancelada, bloqueada ou com erro | `RunCompleted`, `RunStopped`, `RunCancelled`, `RunFailed` ou `FatalError` | terminaliza streaming, fecha usage/activity; falhas adicionam Error block (`crates/slim-tui/src/app.rs:583-642`, `crates/slim-tui/src/app.rs:903-923`) | success limpo; stopped/cancelled em toast; erro em `✕`; footer troca cancel por exit | mesmos outcomes, com truncamento por espaço | o CLI conflui outcomes bloqueados em `RunStopped` (`crates/slim-cli/src/tui.rs:1486-1493`); a TUI os apresenta como `Cancelled`, sem lifecycle `Blocked` (`crates/slim-tui/src/app.rs:595-607`) |
| 16 | Input / approval requerido | `InputRequired`; `ApprovalRequired` vira Notification (`crates/slim-tui/src/api.rs:474-477`) | input só muda para `WaitingForInput` se não há terminal tail; approval só gera toast (`crates/slim-tui/src/app.rs:871-875`) | rail pode dizer `Waiting for input`; não existe ação tipada de resposta (`crates/slim-tui/src/api.rs:507-532`) | sem rail, pergunta/fase vira `Working…` genérico | request ID, pergunta, opções, persistência e approve/reject/answer; não há produtores atuais confirmados |

## 5. Fatos confirmados no código

### 5.1 Eventos e lifecycle

- O core tem `ReasoningDelta`, mas não `ThinkingStarted`/`ThinkingEnded`: `crates/slim-core/src/events.rs:18-32`.
- O contrato normativo mostra explicitamente `ThinkingStarted`, `ThinkingDelta`, `ThinkingEnded`: `Documentações - Projeto/DESIGN-SLIM-TUI.md:512-527`.
- A TUI também possui apenas `ThinkingDelta`: `crates/slim-tui/src/api.rs:273-325`.
- Sem terminal tail, o primeiro `AssistantDelta` fecha o Thinking streaming encontrado e entra em `Responding`; após terminal, não revive fase/lifecycle: `crates/slim-tui/src/app.rs:663-690`.
- `ThinkingDelta` cria bloco recolhido e concatena apenas quando o último bloco é um Thinking compatível: `crates/slim-tui/src/app.rs:709-731`.
- `AssistantEnded` fecha o assistant, mas não limpa `working`/activity; o outcome posterior faz isso: `crates/slim-tui/src/app.rs:695-707` e `crates/slim-tui/src/app.rs:583-642`.

### 5.2 Tools

- O core preserva `id`, `name` e `arguments` em `ProviderToolCall`: `crates/slim-core/src/events.rs:46-64` e `crates/slim-core/src/runtime/mod.rs:1480-1517`.
- O bridge descarta os eventos preliminares `ToolCall`/`ProviderToolCall`; a mesma chamada se torna visível depois, no boundary do executor `ToolStarted`: `crates/slim-tui/src/api.rs:456-466` e `crates/slim-core/src/runtime/mod.rs:1074-1095`.
- O output vira apenas a primeira linha, limitada a 512 caracteres: `crates/slim-tui/src/api.rs:461-466` e `crates/slim-tui/src/api.rs:498-504`.
- `ToolState` guarda somente `name` e `preview`: `crates/slim-tui/src/block.rs:11-15`.
- Cada `ToolStarted` cria um bloco; progress/end procuram a última Tool streaming pelo nome, sem call ID: `crates/slim-tui/src/app.rs:815-855`.
- `ContentPageLoaded` gera apenas uma notificação; não expande conteúdo: `crates/slim-tui/src/app.rs:891-898`.
- O contrato exige ID, args resumidos, duração, `ContentHandle` e expansão inline: `Documentações - Projeto/DESIGN-SLIM-TUI.md:936-951` e `Documentações - Projeto/DESIGN-SLIM-TUI.md:976-982`.
- Calls recebidas juntas são executadas serialmente: `crates/slim-core/src/runtime/mod.rs:789-830`; o mapa de contrato/arquitetura-alvo, que não se declara inventário do wiring atual, descreve batches seguros paralelos: `Documentações - Projeto/RUST-CLI.md:3-4` e `Documentações - Projeto/RUST-CLI.md:257-260`.
- Tools completas, consecutivas e de mesmo nome são agregadas apenas na apresentação: `crates/slim-tui/src/render.rs:175-200` e `crates/slim-tui/src/runtime.rs:860-877`.

### 5.3 Interação e fases

- `UiCommand` não contém answer/approve/reject nem cancelamento individual: `crates/slim-tui/src/api.rs:507-532`.
- `Action` não possui foco/toggle de bloco: `crates/slim-tui/src/reducer.rs:12-30`.
- `Enter` atua no composer/overlays; não expande reasoning/tool: `crates/slim-tui/src/reducer.rs:211-240`.
- `ToolEnded` muda para `Responding` somente quando encontrou bloco streaming homônimo, a tool era a activity atual, a run segue trabalhando e não existe terminal tail: `crates/slim-tui/src/app.rs:843-863`.
- O teste atual cristaliza essa transição, sem representar a espera do provider: `crates/slim-tui/tests/motion_activity_golden.rs:106-125`.
- A degradação remove ActivityRail antes do composer: `crates/slim-tui/src/layout.rs:67-97`; abaixo de `40×8`, ela sempre some: `crates/slim-tui/src/layout.rs:164-235`.
- Quando a rail some, o footer usa `Working…`, sem fase específica: `crates/slim-tui/src/runtime.rs:1640-1661`.
- Spinner/caret só agendam motion tick se sua região/célula estiver visível; reduced motion usa status tick de 1 s apenas quando o elapsed da rail está visível: `crates/slim-tui/src/runtime.rs:668-772` e `crates/slim-tui/src/runtime.rs:2748-2951`.

### 5.4 Streaming e filas

- O coalescer junta apenas `AssistantDelta` adjacente e usage; não junta `ThinkingDelta` nem `ToolProgress`. O reducer ainda concatena Thinking adjacente no mesmo bloco, o que evita uma row por delta sem equivaler a coalescing de eventos: `crates/slim-tui/src/render.rs:35-93` e `crates/slim-tui/src/app.rs:709-731`.
- A janela é armazenada e `window_elapsed()` existe, mas não é consultada pelo runtime: `crates/slim-tui/src/render.rs:95-105`.
- Cada drain cria um coalescer novo de 16 ms e o descarrega incondicionalmente no fim do mesmo drain: `crates/slim-tui/src/runtime.rs:193-230`.
- As lanes expostas à TUI são bounded (`256`/`1024`): `crates/slim-cli/src/tui.rs:460-490`.
- Entre core e projector há `mpsc::channel()` sem capacidade: `crates/slim-cli/src/tui.rs:1369-1384`.
- No Windows o idle bloqueia em `WaitForMultipleObjects`; não há polling periódico quando não existe deadline: `crates/slim-tui/src/runtime.rs:51-87`.

## 6. Achados priorizados

Rubrica editorial desta auditoria: **P1** compromete confiança/inspeção de um fluxo central; **P2** é gap semântico ou contratual relevante, mas sem perda irrecuperável demonstrada; **P3** é risco estrutural ainda não medido. Não são severidades derivadas de SLA ou benchmark fornecido pelo projeto.

### STT-01 — P1 — Tool calls não são auditáveis na TUI

- **Fato:** call ID e argumentos existem no core, mas os eventos preliminares são descartados; a TUI conserva `name + preview` e não materializa output integral (`crates/slim-core/src/events.rs:46-64`; `crates/slim-tui/src/api.rs:456-466`; `crates/slim-tui/src/block.rs:11-15`; `crates/slim-tui/src/app.rs:891-898`).
- **Reprodução:** no buffer VT `80×24` desta sessão, a tool aparece apenas como `✓ shell · exit_code=...`; o comando enviado pela fixture não pode ser visto. Na falha, a mensagem termina na borda.
- **Impacto:** o usuário não consegue confirmar o que rodou, diferenciar calls homônimas, revisar parâmetros perigosos ou abrir o resultado completo.
- **Correção mínima:** transportar `call_id`, resumo sanitizado dos argumentos, duração e `ContentHandle`; ligar `RequestContentPage` a expansão inline do bloco selecionado.
- **Teste necessário:** E2E bridge→reducer→frame com duas calls de mesmo nome/args diferentes; Enter expande output paginado; stale page é ignorada.

### STT-02 — P2 — A fronteira semântica de `Responding` é ambígua após tool

- **Fato:** se `ended && was_current && working && terminal_tail.is_none()`, `ToolEnded` muda a activity para `Responding` antes de qualquer novo delta (`crates/slim-tui/src/app.rs:843-863`). A spec lista `Responding` como fase, mas não define se ela começa no envio do request ou no primeiro byte (`Documentações - Projeto/DESIGN-SLIM-TUI.md:1405-1414`).
- **Reprodução:** fixture concluiu `shell`, aceitou o segundo request e reteve todos os deltas; o binário mostrou `✓ shell` e `◐ Responding · 0s`.
- **Impacto:** por inferência de UX, o usuário pode ler espera de rede/provider como geração ativa; não há violação inequívoca do contrato atual.
- **Correção mínima:** primeiro fechar na spec a fronteira. Se `Responding` significar conteúdo recebido, introduzir `AwaitingProvider` após ToolEnded e entrar em `Thinking`/`Responding` no primeiro delta; se significar request ativo, documentar isso e usar copy menos ambígua.
- **Teste necessário:** clock fake com ToolEnded → silêncio → ReasoningDelta/AssistantDelta; golden deve distinguir os três estados.

### STT-03 — P1 — Reasoning não possui lifecycle causal completo

- **Fato:** não há start/end tipados no core nem na TUI. Silêncio não fecha nada; AssistantDelta e outcome terminal podem fechar lifecycle, enquanto ToolStarted muda a fase sem concluir o bloco Thinking (`crates/slim-core/src/events.rs:18-32`; `crates/slim-tui/src/app.rs:663-676`, `crates/slim-tui/src/app.rs:583-642`, `crates/slim-tui/src/app.rs:815-833`).
- **Reprodução:** em `120×30`, após o único reasoning delta e durante o delay controlado, a rail permaneceu `Thinking`; não existia evento capaz de fechar a fase antes do próximo domínio.
- **Impacto:** lifecycle interno pode ficar streaming além do bloco lógico; elapsed e telemetria de fase não representam o provider com precisão; adapters com fronteira explícita não conseguem projetá-la.
- **Correção mínima:** adicionar os boundaries canônicos `ThinkingStarted`/`ThinkingEnded` e transportá-los no bridge; para protocolos sem boundary nativa, inferir `ThinkingEnded` no primeiro texto/tool/stop, com regra única no normalizer.
- **Teste necessário:** reasoning→answer, reasoning→tool, reasoning-only stop e dois rounds; nenhum Thinking permanece streaming após boundary.

### STT-04 — P1 — Thinking recolhido não é expansível pela UI atual

- **Fato:** o reducer cria Thinking em `Collapsed`; o renderer já possui branch para `Expanded`, mas não existe ação/binding para alternar `FoldState` (`crates/slim-tui/src/app.rs:709-731`; `crates/slim-tui/src/runtime.rs:1243-1262`; `crates/slim-tui/src/reducer.rs:12-30`, `crates/slim-tui/src/reducer.rs:211-240`).
- **Reprodução:** todos os buffers finais desta sessão mantiveram `◌ thinking · <primeira linha>`; Enter não possui rota para o bloco.
- **Impacto:** o conteúdo permanece no estado, mas fica inacessível pela interação atual quando o usuário precisa auditar a decisão do agente.
- **Correção mínima:** seleção de bloco no scrollback + `ToggleBlock`; preview recolhido de até três linhas; expansão inline, sem overlay ou scroll aninhado.
- **Teste necessário:** keyboard golden collapsed→expanded→collapsed, preservando anchor e largura em `120/80/60/40`.

### STT-05 — P2 — Multi-tool perde identidade e diverge da arquitetura-alvo documentada

- **Fato:** o core executa calls em loop serial; a apresentação agrega completas consecutivas pelo nome (`crates/slim-core/src/runtime/mod.rs:789-830`; `crates/slim-tui/src/render.rs:175-200`). `RUST-CLI.md` se identifica como arquitetura-alvo, não inventário atual (`Documentações - Projeto/RUST-CLI.md:3-4`).
- **Reprodução:** no buffer VT `60×16`, duas calls `shell` distintas viraram `✓ shell ×2`; nenhum membro pôde ser inspecionado.
- **Impacto:** duração/ordem aparente podem enganar; calls homônimas ficam indistinguíveis; a TUI não está pronta para paralelismo futuro.
- **Correção mínima:** decidir primeiro o contrato real (serial ou paralelo); em ambos os casos, correlacionar por call ID e permitir expandir o grupo para membros individuais.
- **Teste necessário:** duas calls iguais e duas diferentes, sucesso/falha mistos, ordem original e cancelamento durante a segunda.

### STT-06 — P2 — Input e approval são estados sem saída tipada

- **Fato:** `ApprovalRequired` vira toast; `InputRequired` muda apenas a activity quando não há terminal tail; `UiCommand` não oferece approve/reject/answer e o evento não leva pergunta/opções/ID (`crates/slim-tui/src/api.rs:474-477`, `crates/slim-tui/src/api.rs:507-532`; `crates/slim-tui/src/app.rs:871-875`).
- **Reprodução:** estática, pois não foram encontrados produtores atuais desses eventos no loop executado.
- **Impacto:** quando o fluxo for conectado, a run poderá parecer aguardando uma ação que a própria TUI não consegue enviar.
- **Correção mínima:** não expor o estado visual antes do contrato bidirecional completo; depois adicionar request ID, payload, persistência e comandos tipados.
- **Teste necessário:** request→render→answer/approve→ack, reload e duplicate/stale response.

### STT-07 — P2 — A janela de streaming de 16 ms não governa frames

- **Fato:** `window_elapsed()` não é usado; o coalescer nasce e é descarregado no mesmo drain; reasoning/tool progress não são coalescidos (`crates/slim-tui/src/render.rs:35-105`; `crates/slim-tui/src/runtime.rs:193-230`).
- **Reprodução:** inspeção direta do runtime. Não foi medido impacto de CPU.
- **Impacto:** a implementação não materializa a janela temporal prometida. Mais reducers/frames/CPU em burst é hipótese plausível, mas não foi medido nesta sessão.
- **Correção mínima:** manter coalescer entre iterações, agendar deadline de 16 ms após o primeiro frame e coalescer por stream/entity ID sem perder lifecycle.
- **Teste necessário:** fake clock + burst intercalado de assistant/reasoning/tool; contar frames, latência máxima e ordem dos terminais.

### STT-08 — P3 — Risco não medido na fila core→projector ilimitada

- **Fato:** control/data lanes são bounded, mas a fila anterior usa `mpsc::channel()` ilimitado; o projector pode bloquear/retry na lane cheia (`crates/slim-cli/src/tui.rs:460-490`, `crates/slim-cli/src/tui.rs:1369-1384`).
- **Reprodução:** mecânica confirmada no código; os testes de 1.200 deltas provam completude, não uso máximo de memória (`crates/slim-cli/tests/tui_bridge.rs:337-427`).
- **Impacto:** crescimento de memória/latência sob producer mais rápido que a TUI é risco arquitetural, não regressão reproduzida; a magnitude atual não foi medida.
- **Correção mínima:** tornar a fila intermediária bounded e definir backpressure/coalescing antes da alocação ilimitada.
- **Teste necessário:** produtor sustentado, consumidor retido, limite de fila/RSS e cancelamento causal.

## 7. Divergências entre código, spec e testes

| Tema | Spec | Código atual | Teste atual / lacuna |
|---|---|---|---|
| Reasoning lifecycle | start/delta/end (`Documentações - Projeto/DESIGN-SLIM-TUI.md:512-529`) | só delta; fechamento implícito (`crates/slim-tui/src/api.rs:273-325`) | motion testa mudança por AssistantDelta, não boundary real (`crates/slim-tui/tests/motion_activity_golden.rs:73-125`) |
| Thinking collapsed | recolhido/expansível (`Documentações - Projeto/DESIGN-SLIM-TUI.md:612-613`), preview de 3 linhas (`Documentações - Projeto/DESIGN-SLIM-TUI.md:923-929`) e `ToggleBlock` genérico (`Documentações - Projeto/DESIGN-SLIM-TUI.md:703-716`) | uma linha; sem foco/toggle (`crates/slim-tui/src/runtime.rs:1243-1262`; `crates/slim-tui/src/reducer.rs:12-30`) | não há golden de expansão real |
| Tool block | ID, args, duração, status, ContentHandle (`Documentações - Projeto/DESIGN-SLIM-TUI.md:936-982`) | name + primeira linha (`crates/slim-tui/src/block.rs:11-15`; `crates/slim-tui/src/api.rs:456-466`) | testes validam lifecycle nominal, não inspeção |
| Fase pós-tool | lista `Responding`, mas não define sua fronteira inicial (`Documentações - Projeto/DESIGN-SLIM-TUI.md:1405-1414`) | ToolEnded→Responding sob quatro condições (`crates/slim-tui/src/app.rs:843-863`) | teste afirma a transição sem provider gap (`crates/slim-tui/tests/motion_activity_golden.rs:106-125`) |
| Multi-tool | arquitetura-alvo, não inventário atual, prevê batch seguro paralelo (`Documentações - Projeto/RUST-CLI.md:3-4`, `Documentações - Projeto/RUST-CLI.md:257-260`) | loop serial; aggregate por nome (`crates/slim-core/src/runtime/mod.rs:789-830`; `crates/slim-tui/src/render.rs:175-200`) | fixture preserva calls, mas não valida inspeção individual |
| Streaming | primeiro delta imediato; seguintes ≤16 ms (`Documentações - Projeto/DESIGN-SLIM-TUI.md:822-829`) | flush por drain; janela não usada (`crates/slim-tui/src/runtime.rs:193-230`) | teste de 1.200 deltas valida completude, não cadência/frame count (`crates/slim-cli/tests/tui_bridge.rs:337-427`) |
| Filas | stream/control bounded e lossless (`Documentações - Projeto/DESIGN-SLIM-TUI.md:763-804`) | fila core→projector unbounded (`crates/slim-cli/src/tui.rs:1369-1384`) | sem assert de capacidade/RSS |
| Input/approval | views e comandos tipados (`Documentações - Projeto/DESIGN-SLIM-TUI.md:535-569`) | input sem payload; approval toast (`crates/slim-tui/src/api.rs:474-477`, `crates/slim-tui/src/api.rs:507-532`) | sem E2E bidirecional |
| ConPTY | matriz golden em `Documentações - Projeto/DESIGN-SLIM-TUI.md:2064-2095`; PTY E2E em `Documentações - Projeto/DESIGN-SLIM-TUI.md:2097-2110` | existe um teste `80×24` básico (`crates/slim-cli/tests/tui_pty.rs:50-66`) | C4 consta fechado e C5 pendente no tracker (`Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md:383-386`); teste está `#[ignore]` e usa `CARGO_BIN_EXE_slim` |

O `crates/slim-tui/tests/golden_matrix.rs:83-116` percorre tamanhos, mas comprova principalmente ausência de branding/panic, composer, usage e tiling. Não cobre sozinho a matriz normativa de `Documentações - Projeto/DESIGN-SLIM-TUI.md:2064-2095`. O gate PTY continua explicitamente pendente no tracker: `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md:385-386`; `crates/slim-cli/tests/tui_pty.rs:50-66` fixa `80×24`, está ignorado e usa o executável construído pelo Cargo.

### Hipóteses obrigatórias verificadas

1. **Confirmada — não existe `ThinkingEnded` explícito.** Core e TUI expõem delta, sem start/end: `crates/slim-core/src/events.rs:18-32`; `crates/slim-tui/src/api.rs:273-325`.
2. **Confirmada — `Thinking` persiste durante silêncio.** O delta entra na fase e nenhum timer a encerra; o próximo evento de outro domínio ou o outcome é que altera/terminaliza estado: `crates/slim-tui/src/app.rs:663-731`, `crates/slim-tui/src/app.rs:583-642`.
3. **Confirmada com condição — `AssistantDelta` encerra Thinking e muda para `Responding`.** Isso ocorre somente sem terminal tail; deltas tardios não revivem a run: `crates/slim-tui/src/app.rs:663-690`.
4. **Confirmada com condição — `ToolStarted` muda para `Running <nome>`.** Cada start cria bloco e, se o lifecycle ainda é streaming, ativa `RunningTool(name)`: `crates/slim-tui/src/app.rs:815-833`; `crates/slim-tui/src/runtime.rs:1049-1059`.
5. **Confirmada com condição — `ToolEnded` pode voltar antes de novo texto para `Responding`.** Exige `ended && was_current && working && terminal_tail.is_none()`: `crates/slim-tui/src/app.rs:843-863`.
6. **Confirmada — Thinking nasce `Collapsed` e não há interação atual para expandi-lo.** O renderer suporta estado expanded, mas reducer/teclas não oferecem toggle: `crates/slim-tui/src/app.rs:709-731`; `crates/slim-tui/src/runtime.rs:1243-1262`; `crates/slim-tui/src/reducer.rs:12-30`, `crates/slim-tui/src/reducer.rs:211-240`.
7. **Parcial — `ProviderToolCall`/`ToolCall` não geram UI preliminar.** Esses eventos são descartados, mas a mesma execução aparece depois em `ToolStarted`: `crates/slim-tui/src/api.rs:456-466`; `crates/slim-core/src/runtime/mod.rs:1074-1095`.
8. **Confirmada — argumentos da tool não são exibidos.** O core os possui, porém `ToolState` recebe apenas name/preview: `crates/slim-core/src/events.rs:46-64`; `crates/slim-tui/src/block.rs:11-15`; `crates/slim-tui/src/api.rs:456-466`.
9. **Confirmada no caminho core→TUI normal — preview usa somente a primeira linha e até 512 caracteres.** `UiEvent::from_core` chama `bounded_first_line`; a variante pública `ToolProgress` em si aceita String sem esse cap: `crates/slim-tui/src/api.rs:318-322`, `crates/slim-tui/src/api.rs:461-504`.
10. **Confirmada com escopo — tools completas, consecutivas e homônimas são agrupadas em `✓ nome ×N`.** A agregação é somente de apresentação: `crates/slim-tui/src/render.rs:175-200`; `crates/slim-tui/src/runtime.rs:860-877`.
11. **Confirmada e normativa — terminal baixo pode remover a ActivityRail e usar `Working…`.** É degradação deliberada, não achado: `crates/slim-tui/src/layout.rs:67-97`, `crates/slim-tui/src/layout.rs:164-235`; `crates/slim-tui/src/runtime.rs:1640-1661`; `Documentações - Projeto/DESIGN-SLIM-TUI.md:1195-1198`, `Documentações - Projeto/DESIGN-SLIM-TUI.md:1241-1248`.
12. **Confirmada — a janela de 16 ms não governa o loop atual.** Ela é armazenada, mas o runtime cria e descarrega o coalescer no mesmo drain sem consultar `window_elapsed()`: `crates/slim-tui/src/render.rs:95-105`; `crates/slim-tui/src/runtime.rs:193-230`.
13. **Parcial — a equivalência de `ThinkingDelta` é refutada no coalescer, mas existe na projeção do bloco.** O `EventCoalescer` não o une, embora o reducer concatene deltas adjacentes no mesmo Thinking: `crates/slim-tui/src/render.rs:35-93`; `crates/slim-tui/src/app.rs:709-731`.
14. **Parcial — ticks periódicos de caret, spinner e elapsed respeitam visibilidade; redraws por evento não consultam essa visibilidade.** Motion/elapsed usam predicates visuais, mas qualquer evento reduzido marca `dirty` e desenha: `crates/slim-tui/src/runtime.rs:668-772`, `crates/slim-tui/src/runtime.rs:274-350`, `crates/slim-tui/src/runtime.rs:2748-2951`. CPU não foi medida.

### Verificações adicionais que evitaram falsos positivos

- **Novo reasoning pós-tool não mistura no bloco anterior.** Como Tool é o último bloco, o próximo delta cria outro Thinking (`crates/slim-tui/src/app.rs:709-731`); o buffer `80×24` desta sessão mostrou os dois blocos.
- **Cancelamento não apareceu como falha na sequência observada.** O binário mostrou `■ shell · cancelled`, distinto de `✕ read · failed`. Isso não é garantia para toda ordenação: `RunCancelled` só altera blocos ainda streaming; um `ToolEnded(false)` já reduzido permanece Failed (`crates/slim-tui/src/app.rs:609-619`, `crates/slim-tui/src/app.rs:843-864`).
- **O executor normal fecha a tool.** Emite Started → Output → Finished, inclusive quando detecta cancelamento após a execução: `crates/slim-core/src/runtime/mod.rs:1074-1132`.
- **NO_COLOR não emitiu cor no comportamento capturado.** O buffer `32×10` teve `0` SGR de cor. A conversão estática de `ColorDepth::None` para ANSI16 (`crates/slim-tui/src/theme.rs:129-135`) não foi demonstrada como bug visível no backend implantado.
- **Tools do mesmo response não rodam em paralelo hoje.** O core as serializa (`crates/slim-core/src/runtime/mod.rs:789-830`); por isso não foi atribuído bug de corrida visual atual.

## 8. Direção visual recomendada

**Uma única direção:** manter transcript minimalista + uma ActivityRail, mas transformar cada row em uma timeline causal com progressive disclosure. Não adicionar dashboard, cards, painel lateral permanente ou segunda fonte de status.

### Wide — durante e após tool

```text
  ◌ thinking · 1.2s
    Primeiro verifico os eventos...
    Depois comparo o contrato...
    Enter expand

  ◌ shell  #call-a · running · 0.8s
    Start-Sleep -Milliseconds 1500…
    Enter details                         Ctrl+C stop run

  ✓ shell  #call-a · 1.6s
    exit 0 · fixture-tool-ok              Enter details

  ◌ Waiting for provider · 0.3s           Ctrl+C stop
```

Quando chegar reasoning:

```text
  ◌ thinking · 0.0s
    A ferramenta terminou; agora sintetizo.
```

### Grupo de tools

```text
  ✓ 2 tools · 1.8s
    ✓ shell  #call-a · exit 0
    ✓ shell  #call-b · exit 0
    Enter collapse
```

### Narrow

```text
40×10
◒ Runni · 0s
^C          ctx ~3% · ↑0 ↓0

<40×8 · emergência normativa
›
Working… ^C
```

O fallback genérico abaixo de `40×8` é uma decisão normativa (`Documentações - Projeto/DESIGN-SLIM-TUI.md:1195-1198`, `Documentações - Projeto/DESIGN-SLIM-TUI.md:1241-1260`) e está implementado (`crates/slim-tui/src/layout.rs:164-235`; `crates/slim-tui/src/runtime.rs:1652-1661`). Ele preserva cancelamento ao custo deliberado da fase específica; não foi tratado como defeito nem deve mudar sem revisão explícita da spec.

Princípios:

- fase só muda quando existe evento causal;
- glyph + label + status, nunca cor isolada;
- preview suficiente para reconhecer, expansão para auditar;
- call ID curto apenas quando há ambiguidade;
- uma rail transitória; o transcript guarda história;
- em largura suportada, narrow reduz texto sem trocar o estado; no layout de emergência, preservar o fallback normativo e a ação de cancelamento.

## 9. Slices de implementação recomendados

Nenhum slice foi implementado nesta auditoria.

| Ordem | Slice / arquivos principais | Comportamento mínimo | Teste RED | Critério GREEN | Risco |
|---:|---|---|---|---|---|
| 1 | **Lifecycle de Thinking e decisão pós-tool** — `DESIGN-SLIM-TUI.md`, `slim-core/src/events.rs`, `slim-tui/src/api.rs`, `app.rs` | fechar na spec o significado de `Responding`; adicionar `ThinkingStarted/Ended` e, somente se decidido, `AwaitingProvider`; sem mudar layout | sequências reasoning→silêncio→answer/tool/stop e tool→provider gap falham por falta de boundary/fase | cada domínio tem início/fim causal; delta tardio não revive terminal; copy pós-tool corresponde à decisão | alto: compatibilidade de eventos/replay e adapters |
| 2 | **Expansão de Thinking** — `block.rs`, `reducer.rs`, `runtime.rs`, testes motion/scroll | seleção mínima + `ToggleBlock`; collapsed de até três linhas e expanded inline | teclado não consegue alternar bloco; anchor muda no reflow | collapsed→expanded→collapsed funciona em `120/80/60/40`, sem overlay/scroll aninhado | médio: foco versus composer e anchor |
| 3 | **Identidade/detalhes de tool** — `events.rs`, `api.rs`, `block.rs`, `app.rs`, `runtime.rs` | transportar call ID, args sanitizados, duração e `ContentHandle`; `ContentPageLoaded` preenche somente request correspondente | duas calls homônimas/args distintos são indistinguíveis; page stale é aceita ou inútil | membros e outputs paginados são auditáveis; page stale ignorada; segredo redigido antes da UI | alto: redaction, memória e correlação |
| 4 | **Contrato multi-tool** — `DESIGN-SLIM-TUI.md`, `RUST-CLI.md`, `slim-core/src/runtime/mod.rs`, `slim-tui/src/render.rs` | decidir serial versus paralelo; agrupar por turn/run preservando membros por call ID | sucesso/falha mistos e duas homônimas perdem identidade/ordem | ordem determinística, failed/cancelled fora do grupo, expansão mostra cada call | alto se paralelizar; baixo se alinhar docs ao serial |
| 5 | **Input/approval bidirecional** — `events.rs`, `api.rs`, `app.rs`, `reducer.rs`, bridge CLI | request ID + payload + answer/approve/reject + ack/persistência; não mostrar estado antes do round-trip existir | UI entra em espera sem comando capaz de responder | request→render→response→ack funciona; duplicate/stale/reload são determinísticos | alto: segurança de approval e replay |
| 6 | **Medição de cadência/fila** — testes de `render.rs`, `runtime.rs`, `tui_bridge.rs` | fake clock e consumidor retido; nenhuma mudança de produção | ausência de contagem de frames, latência, profundidade e RSS torna impacto desconhecido | baseline repetível registra first frame, frames/burst, p95, queue high-water e RSS | baixo: harness apenas |
| 7 | **Cadência/backpressure, condicional ao slice 6** — `render.rs`, `runtime.rs`, `slim-cli/src/tui.rs` | persistir coalescer entre iterações e limitar fila sem perder eventos terminais | threshold previamente definido falha; burst demonstra excesso/crescimento | primeiro delta imediato; demais ≤16 ms; terminal loss `0`; memória/fila dentro do teto medido | alto: ordering, cancelamento e deadlock |
| 8 | **Harness do binário implantado** — `crates/slim-cli/tests/tui_pty.rs` + fixture offline | aceitar caminho explícito do executável e matriz de dimensões/capabilities | teste atual está `#[ignore]`, fixa `80×24` e usa binário do Cargo | gate reproduz os cenários solicitados nesta auditoria no `Slim.exe` implantado, com buffers persistidos e diff legível; expansão futura cobre PTY M1 (`Documentações - Projeto/DESIGN-SLIM-TUI.md:2097-2110`) | médio: flake de PTY/Windows |

Cada slice deve atualizar primeiro a spec quando alterar comportamento normativo e, após código, cumprir o deploy obrigatório do projeto. Isso não se aplica a esta auditoria somente leitura.

## 10. Performance: provado versus hipótese

### Provado mecanicamente

- Em Windows, idle sem deadline usa espera bloqueante em handles: `crates/slim-tui/src/runtime.rs:51-87`.
- `HeightIndex::build` aloca `Vec`/`HashMap` proporcionais ao número de blocos e varre todos: `crates/slim-tui/src/render.rs:175-232`.
- O frame pode construir o índice duas vezes quando surge scrollbar: `crates/slim-tui/src/runtime.rs:847-853`.
- A altura de Assistant chama `markdown_row_count`, que faz `project(source)`; a materialização chama `render_markdown`, que faz `project(source)` novamente: `crates/slim-tui/src/render.rs:125-146`, `crates/slim-tui/src/markdown.rs:170-185`, `crates/slim-tui/src/markdown.rs:235-318`, e `crates/slim-tui/src/runtime.rs:1326-1389`.
- O cache atual guarda alturas, não uma projeção Markdown: `crates/slim-tui/src/render.rs:307-325`.
- A janela de 16 ms não é aplicada ao loop: `crates/slim-tui/src/render.rs:95-105` e `crates/slim-tui/src/runtime.rs:193-230`.
- O canal core→projector é ilimitado, embora as lanes seguintes sejam bounded: `crates/slim-cli/src/tui.rs:1369-1384` versus `crates/slim-cli/src/tui.rs:460-490`.

### Não provado nesta sessão

- regressão de p95 no hardware atual;
- custo percentual de HeightIndex, Markdown ou alocação;
- crescimento real de RSS/fila em sessão longa;
- CPU idle medida;
- impacto perceptível do coalescing ausente;
- equivalência de performance entre fixture loopback e provider real.

O tracker registra testes e benchmark históricos, mas eles não foram reexecutados e não são tratados como medição atual (`Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md:500-506`).

## 11. Limites, preservações e verificação

### Limites

- Não foi usado provider real/comercial.
- Não foram executados `cargo test`, build, benchmark ou `refresh-slim.ps1`, pois escreveriam fora do único arquivo autorizado e não houve mudança de código.
- Os buffers VT/ConPTY observados nesta sessão evidenciam células/ANSI do binário implantado e os recortes acima; como runner e frames brutos não foram persistidos, não constituem gate reproduzível. Também não provam rasterização de fonte, DPI, antialiasing ou chrome do Windows Terminal.
- Reduced motion em `32×10` não tem sinal animado observável porque a ActivityRail é removida pelo layout; somente a ausência de mudança estrutural e o footer puderam ser observados nesse tamanho.
- Input/approval foram avaliados estaticamente porque não há produtores atuais confirmados no fluxo executado.
- Paralelismo não foi exercitado porque o executor atual é serial.
- Os tempos do runner não são números de performance.

### Não mudar sem nova decisão de spec

- autoridade de domínio no core, não na TUI;
- control/data lanes separadas e terminais causais de cancelamento;
- fullscreen/alternate screen e restauração de terminal;
- uma ActivityRail transitória, sem duplicação permanente;
- composer e operational footer como âncoras de degradação;
- distinção visual `✓` / `✕ failed` / `■ cancelled`;
- NO_COLOR com semântica por glyph/label;
- welcome atual, fora deste escopo;
- primeiro delta imediato;
- sanitização/redaction antes da projeção visual.

### ✅ Verificação de Entrega

- [x] Rodei o que afirmo ter rodado (comando + saída registrada neste relatório)
- [x] Números citados contados nesta sessão (R2)
- [x] Caminho:linha citado foi lido nesta sessão (R3)
- [ ] cargo test --workspace verde (0 failed, 0 warnings) — **N/A nesta auditoria:** não executado; nenhuma mudança de código e a única escrita autorizada era este relatório
- [ ] .\refresh-slim.ps1 executado, imprimiu OK: (R7) — **N/A nesta auditoria:** não executado; o binário não foi modificado e sua identidade foi verificada
- [ ] Docs de status/números atualizados e grep dos antigos vazio (R11) — **N/A nesta auditoria:** comportamento não foi alterado; editar tracker/spec era proibido pelo escopo
- [x] Incertezas e limitações declaradas explicitamente (R4)

**Única escrita desta auditoria:** `D:\Slim\analysis_outputs\AUDIT-STREAMING-THINKING-TOOLS-TUI.md`.
