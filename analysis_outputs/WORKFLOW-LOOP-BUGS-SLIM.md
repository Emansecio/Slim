# Workflow iterativo de auditoria, correção e reauditoria — Slim

**Data:** 2026-08-26  
**Fonte de verdade:** checkout local `D:\Slim`  
**Estado:** concluído

## 0. Método e limites

- `RULES.md`, `AGENTS.md`, o handoff anexado e este relatório foram lidos
  integralmente antes das ações.
- Cada achado foi revalidado no checkout atual, reproduzido por RED, corrigido
  pelo menor patch e provado por GREEN antes da passagem seguinte.
- A auditoria usou somente filesystem local, fixtures e servidores loopback;
  nenhum provider comercial/real foi chamado.
- Nenhuma operação Git proibida ou limpeza destrutiva foi usada. O WIP externo
  permaneceu no lugar e continuou mudando durante o trabalho.

## 1. Baseline revalidado

### Worktree e toolchain

- Snapshot antes das correções: 183 entradas (`89` tracked, `94` untracked).
- Snapshot final após documentação/deploy: 191 entradas
  (`97` tracked, `94` untracked). O delta inclui o próprio workflow e WIP
  concorrente; nada foi revertido.
- Último `git diff --stat` observado: 97 arquivos, 26.851 inserções e 3.190
  remoções.
- Nenhum processo `cargo`, `rustc`, `rustdoc` ou `slim` ficou remanescente.
- Toolchain canônico: `rustc 1.97.1` / `cargo 1.97.1`, MSVC Scoop.
- Escopo varrido: 187 arquivos Rust ativos/testes (`crates/` + `tests/`) e 5
  scripts de projeto; POCs fora do workspace foram classificados à parte.

### Gate fresco anterior às correções

- `cargo fmt --all -- --check`: exit `0`.
- `cargo check --workspace`: exit `0`.
- `RUSTFLAGS=-C debuginfo=1 cargo test --workspace -j 1 --no-fail-fast`:
  exit `0`.
- `cargo clippy --workspace --all-targets -- -D warnings`: RED, exit `101`,
  `clippy::manual_contains` em `search.rs`.

### Binário inicial

`target\release\slim.exe` e `C:\Users\User\bin\Slim.exe` eram idênticos:
12.525.056 bytes, SHA-256
`8366880BDB259C4C2FFBD983569641D5AF8A02A90B2967E21A269E04FE500016`;
`slim --version` retornou `slim 0.1.0`.

## 2. Ciclos RED → GREEN

Foram concluídos 14 ciclos: **P1: 0, P2: 10, P3: 4**.

1. **P2 — shell bounded:** RED capturou 8.392.704 bytes contra teto esperado
   de 8.388.608. GREEN limita cada stream bruto a 8 MiB, drena o restante em
   paralelo e contabiliza descarte bruto + contextual no marcador final.
2. **P3 — Clippy integral:** `manual_contains` reproduzido; troca cirúrgica por
   `SKIP_EXTENSIONS.contains(...)`; o gate integral ficou verde.
3. **P3 — fila de prompts:** RED aceitou o nono prompt durante run ativo.
   GREEN limita a FIFO a 8 e preserva o nono no draft com aviso.
4. **P2 — list:** RED mostrou materialização sem limite. GREEN aplica teto bruto
   de 10.000 entradas, página padrão 200/máxima 500 e cursor determinístico.
5. **P2 — read:** RED mostrou arquivo/página sem teto. GREEN usa leitura
   streaming, arquivo máximo 10 MiB e página máxima 1 MiB/4.096 linhas.
6. **P2 — search hit:** RED permitiu linha arbitrária. GREEN limita texto por
   hit a 8 KiB e o fast path `rg` a arquivos de 10 MiB.
7. **P3 — terminal Windows:** RED com CP de entrada 437 e saída 1252 provou que
   ambas eram restauradas como 1252. GREEN salva/restaura as duas separadamente.
8. **P2 — write/patch:** RED aceitou conteúdo/precondição oversized. GREEN
   limita entrada, arquivo atual e resultado a 10 MiB.
9. **P2 — skills:** RED provou leitura integral de body/metadata. GREEN limita
   frontmatter streaming a 64 KiB e body a 1 MiB sem decodificação antecipada.
10. **P2 — config:** RED aceitou `slim.toml` arbitrário. GREEN limita a 1 MiB.
11. **P2 — auth:** RED alcançou alocação pelo tamanho do arquivo. GREEN rejeita
    auth nativa acima de 1 MiB antes de materializar.
12. **P2 — stdin:** RED mostrou stdin headless ilimitado e erro ignorado. GREEN
    limita a 8 MiB e retorna erro explícito/exit 11.
13. **P3 — fast path `rg`:** RED descartou hits `C:\...` ao separar por `:`.
    GREEN usa `--null`, framing NUL, globs corretos e `--sort path`; o teste antes
    ignorado agora é ativo.
14. **P2 — composer:** RED de 50.000 inserts ultrapassou 30 s. GREEN mantém
    `char_count` incremental, coalesce de texto e evita clone total no slash
    lookup; o mesmo teste concluiu em aproximadamente 0,01 s.

Resultado líquido: **16 testes novos** e o teste `rg` promovido de ignored para
ativo. A suíte passou de 779 passed / 2 ignored para 796 passed / 1 ignored.

## 3. Reauditoria global posterior às correções

A passagem final posterior ao ciclo 14 cobriu:

- panics, `unwrap`/`expect` alcançáveis e invariantes;
- I/O, filesystem, parsing/serialização e arquivos locais;
- subprocessos, timeout, cancelamento, drenagem e status;
- buffers, canais, filas, caches e crescimento sem teto;
- lifecycle TUI/CLI, terminal Windows e composer;
- persistência, sessões, recovery e artifacts;
- todos os adapters por meio do cliente HTTP compartilhado e fixtures offline;
- testes ignorados, scripts, benches e POCs fora do runtime ativo.

**Resultado:** nenhuma descoberta confirmada nova. O único teste ignorado é o
gate físico ConPTY, que depende do host.

## 4. Descartes revalidados

- `unreachable!` nos geradores de sufixo exige esgotar todo `usize`; não é
  achado prático reproduzível.
- `JsonLineFramer`, `run_shell` sem timeout e o decoder manual de bracketed paste
  não têm caller no runtime ativo.
- Recovery legado público e artifact reads observados são superfícies históricas
  ou recebem somente dados já bounded; não houve reprodução de dano atual.
- Canais de input físico são limitados pelas lanes 256/1.024; transcript longo
  não tem política normativa de perda e não foi reportado sem reprodução.
- Durable session/provider cache/stream/SSE já possuem tetos; todos os providers
  usam a política comum de connect/idle/wall e stream 64 MiB/SSE 1 MiB.
- Caminhos absolutos e modo Auto são decisões normativas. POCs fora do workspace
  e benchmark `.task-v2` não são runtime ativo.

## 5. Verificação executada antes do gate de entrega

- Full suite após as correções: `EXIT=0`, 82 suítes, 796 passed, 0 failed,
  1 ignored.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`.
- Bench `tool_setup`: mediana 11.415 ns/turno; cache 0,625 ns; speedup medido
  18.264,1× (corpus local, não extrapolado para provider).
- Bench `long_session`: 3.200 blocos / 4.968.694 bytes; scroll p95 1,269 ms e
  input→frame p95 1,449 ms, ambos abaixo do budget local de 16 ms.

## 6. Gate final e identidade implantada

- `cargo fmt --all -- --check`: exit `0`.
- `cargo check --workspace`: exit `0`.
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`.
- `git diff --check`: exit `0`.
- Grep dos números antigos nos Markdown: somente linhas históricas do tracker e
  a frase comparativa 779→796 deste relatório; nenhum status corrente stale.
- `.\refresh-slim.ps1 -Test`: exit `0`; imprimiu
  `OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe`.

| Caminho | Tamanho | LastWriteTime | SHA-256 |
|---|---:|---|---|
| `D:\Slim\target\release\slim.exe` | 12.571.648 B | `2026-08-26T17:55:36.8094289-03:00` | `32A8E7065A0DB44CEC65BA2827C00B57DAA598948611F0A2D62DB78A70478472` |
| `C:\Users\User\bin\Slim.exe` | 12.571.648 B | `2026-08-26T17:55:36.8094289-03:00` | `32A8E7065A0DB44CEC65BA2827C00B57DAA598948611F0A2D62DB78A70478472` |

`slim --version` retornou `slim 0.1.0`, exit `0`. A inspeção encontrou somente
esses dois executáveis canônicos (um no target e a cópia implantada); não há
versão paralela antiga em `C:\Users\User\bin` nem no topo de `target\release`.
A cópia implantada anterior foi substituída atomicamente pelo build atual.

## 7. Limitações reais

- Nenhum provider real foi exercitado; latência/custo/fidelidade de rede não
  fazem parte desta prova.
- O ConPTY físico continua ignorado neste host; fixtures TUI/PTY offline e
  TestBackend passaram, mas não substituem matriz manual de terminal/IME/mouse.
- O WIP grande e concorrente impede atribuir todo o diff a este workflow; o
  snapshot final será registrado sem tentar normalizá-lo.

## 8. Extensão de hardening operacional — G299–G305

Uma nova rodada revalidou e corrigiu **7 achados confirmados** em **8 ciclos
RED → GREEN**, adicionando **9 testes**:

1. SSE compartilhado preserva UTF-8 dividido entre chunks (`ol��` → `olá`) e
   rejeita linha inválida sem conversão lossy;
2. callback OAuth lê request fragmentada até `CRLFCRLF`, limita 16 KiB e observa
   timeout/cancelamento dentro da conexão aceita;
3. bodies OAuth Anthropic/Codex são limitados a 64 KiB antes do JSON;
4. catálogo ClinePass é drenado em chunks com teto bruto de 1 MiB;
5. sessão legada v1 aplica o teto durável de 64 MiB e detecta crescimento;
6. `--session` abre/valida o writer antes de provider ou tools;
7. falha de raw-mode reverte o estado de console já alterado;
8. falha de write/replace remove `.slim-*.tmp` sem mascarar o erro original.

A reauditoria posterior cobriu **183 arquivos Rust** e **5 scripts**, incluindo
panics, I/O/filesystem, subprocessos, cancelamento/timeout, buffers/canais,
caches, lifecycle TUI/CLI, persistência, parsing e limites de provider. Nenhum
achado confirmado novo permaneceu.

### Gate e benchmark local

- `cargo fmt --all -- --check`: exit `0`;
- `cargo check --workspace`: exit `0`;
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`;
- `cargo test --workspace -j 1 --no-fail-fast`: 82 suítes, **830 passed**,
  0 failed, 1 ignored (ConPTY físico), exit `0`;
- parser SSE: 20.000 linhas completas em 46.765.000 ns no profile de teste
  local; medida diagnóstica, não extrapolada para provider;
- `git diff --check`: exit `0`;
- `.\refresh-slim.ps1 -Test`: exit `0`, imprimiu `OK:`.

| Caminho | Tamanho | LastWriteTime | SHA-256 |
|---|---:|---|---|
| `D:\Slim\target\release\slim.exe` | 13.020.160 B | `2026-08-26T21:19:30.6202472-03:00` | `590E6493D12B453BA1DBA4FA97A174B939DDB1C7BC144F6FB821C3453B5CF82F` |
| `C:\Users\User\bin\Slim.exe` | 13.020.160 B | `2026-08-26T21:19:30.6202472-03:00` | `590E6493D12B453BA1DBA4FA97A174B939DDB1C7BC144F6FB821C3453B5CF82F` |

`slim --version` retornou `slim 0.1.0`, exit `0`. Há somente um `Slim*.exe`
canônico no PATH e um no topo de `target\release`; a cópia anterior foi
substituída pelo deploy. Snapshot final do WIP: **197 entradas**, 101 tracked e
96 untracked; nenhuma normalização ou reversão foi executada.

Limitações: nenhum provider real/comercial foi chamado e o teste ConPTY físico
permanece ignorado neste host.

## 9. Reauditoria de refinamento TUI/CLI — G306–G311

Uma revalidação posterior encontrou seis regressões/rebarbas confirmadas no
checkout corrente e as tratou em cinco ciclos RED → GREEN:

1. o runtime havia voltado ao `poll(16 ms)` e ignorava o `WakeSignal`;
2. a lane de stream drenava sem teto e budgets esgotados não rearmavam o wake;
3. o frame de animação avançava por poll, não pelo tempo monotônico;
4. ao remover polling, toast ocioso precisou de deadline próprio de expiração;
5. seletores `›` e duas mensagens em português destoavam da superfície;
6. `slim --help` era uma linha monolítica sem agrupamento operacional.

A correção adicionou **7 testes**: quatro para relógio/deadline/wake, dois para
os lotes 32/1.024 e um para expiração de toast sem polling. Goldens de Thinking,
tools e emergência, além do contrato exato do help, foram atualizados.

### Gate, benchmark e binário implantado

- `cargo fmt --all -- --check`: exit `0`;
- `cargo check --workspace`: exit `0`;
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`;
- `cargo test --workspace -j 1 --no-fail-fast`: 82 suítes, **837 passed**,
  0 failed, 1 ignored (ConPTY físico), exit `0`;
- bench release `long_session`: 3.200 blocos / 4.968.694 bytes,
  input→frame p95 **1,132 ms** e scroll p95 **0,794 ms**, teto 16 ms;
- `git diff --check`: exit `0`;
- `refresh-slim.ps1 -Test`: exit `0`, imprimiu `OK:`.

| Caminho | Tamanho | LastWriteTime | SHA-256 |
|---|---:|---|---|
| `D:\Slim\target\release\slim.exe` | 13.029.376 B | `2026-08-26T22:46:24.5616578-03:00` | `AC2191BF2781812DE29EC437161A192AE6280F758454A65AECDA4759E633ED38` |
| `C:\Users\User\bin\Slim.exe` | 13.029.376 B | `2026-08-26T22:46:24.5616578-03:00` | `AC2191BF2781812DE29EC437161A192AE6280F758454A65AECDA4759E633ED38` |

`slim --version` retornou `slim 0.1.0`, exit `0`. Existe um único
`Slim*.exe` no topo de `target\release` e um no PATH; a cópia estática anterior
foi substituída. Snapshot final comparável do WIP: **200 entradas**, 101 tracked
e 99 untracked; nenhuma alteração existente foi normalizada ou revertida.

Limitações reais: nenhum provider comercial foi chamado; o ConPTY físico segue
ignorado. O companion visual local permanece aberto por solicitação do usuário.

## 10. Extensão de troca de provider — G314–G315

A correção inicial G314 cobria somente OpenCode Go → Codex. A revalidação
seguinte confirmou a assimetria restante: `SetOpenCodeModel`,
`SetClinePassModel` e `SetCommandCodeModel` aceitavam apenas a request do mesmo
provider e ignoravam as API keys irmãs já persistidas.

O RED offline `model_overlay_switches_across_all_saved_provider_groups` falhou
em Codex → OpenCode Go. O GREEN usa uma rota compartilhada para ativar
atomicamente a API key salva, reconstruir a request, limpar a sessão OAuth
anterior e persistir o destino. O teste percorre Codex → OpenCode Go →
ClinePass → Command Code → Codex e verifica `active_provider` em cada
passagem. Nenhum provider real foi chamado.

### Gate e binário implantado

- `cargo fmt --all -- --check`: exit `0`;
- `cargo check --workspace`: exit `0`;
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`;
- `cargo test --workspace -j 1 --no-fail-fast`: 82 suítes, **841 passed**,
  0 failed, 1 ignored (ConPTY físico), exit `0`;
- `git diff --check`: exit `0`;
- `refresh-slim.ps1 -Test`: exit `0`, imprimiu `OK:`;
- target/PATH: 13.044.224 bytes, SHA-256
  `6D936DBDEB6D50E716DBB26778A5C795790E7A5D77AE3170D7C250E4913A8F58`;
- `slim --version`: `slim 0.1.0`, exit `0`;
- companion local: HTTP 200 e mantido aberto.

O escopo do overlay continua sendo os quatro grupos normativos. Anthropic e
OpenAI-compatible continuam suportados como providers, mas não possuem grupo
de modelos no `/model`; nenhuma superfície nova foi inventada nesta correção.
Snapshot final do WIP: 200 entradas, 101 tracked e 99 untracked, sem reset,
restore, checkout, clean, stash, pull, fetch ou reversão de trabalho externo.

## 11. Regressão de tool call fantasma — G316

O screenshot foi produzido pelo `C:\Users\User\bin\Slim.exe` iniciado depois do
deploy anterior e confirmou uma segunda forma do erro: o provider entregava
texto válido, depois um delta Chat Completions com `arguments:null`, e encerrava
o turno com `finish_reason:stop`. O parser convertia `null` em fragmento vazio,
mas o normalizador tentava publicar esse buffer como ferramenta no stop normal.

O RED offline
`free_provider_bridge_discards_null_tool_placeholder_on_normal_stop` reproduziu
`MalformedToolCall` após aceitar o texto. O GREEN limita a publicação de deltas
Chat Completions a terminais `tool_calls`/`function_call`; `stop` descarta o
placeholder sem executar side effect. Tool calls exigidas continuam validando
identidade, nome e JSON. O E2E
`codex_subscription_responses_execute_tool_and_send_function_output` prova que
Codex/Responses continua publicando calls válidas no terminal `completed`.

### Gate e binário implantado

- RED focado: 1 failed com `MalformedToolCall`;
- GREEN focado: 6 testes Chat Completions + 1 E2E Codex, 0 failed;
- `cargo fmt --all -- --check`: exit `0`;
- `cargo check --workspace`: exit `0`;
- `cargo clippy --workspace --all-targets -- -D warnings`: exit `0`;
- `cargo test --workspace -j 1 --no-fail-fast`: 82 suítes, **842 passed**,
  0 failed, 1 ignored (ConPTY físico), 0 warnings, exit `0`;
- `refresh-slim.ps1 -Test`: exit `0`, imprimiu `OK:`;
- target/PATH: 13.044.224 bytes, SHA-256
  `C0334DF4AA5FFECBDD7047E5CFE87843760EAB97653832A0380225272FCFD6E6`;
- `slim --version`: `slim 0.1.0`, exit `0`;
- companion local: HTTP 200 e mantido aberto.

Snapshot final do WIP: 200 entradas, 101 tracked e 99 untracked. Nenhum reset,
restore, checkout, clean, stash, pull, fetch ou provider real foi usado.

## 12. Hardening terminal-authoritative de tool calls — G317

A reauditoria de G316 confirmou que o parser OpenAI-compatible ainda podia
retornar `MalformedToolCall` antes de receber `finish_reason` para outras formas
de payload: `tool_calls:[null]`, `[{}]`, `function:null`, container/tipo/índice/
nome/argumentos inválidos e fragmentos conflitantes. Isso permitia repetir o
erro após texto válido. Um irmão nulo posterior a uma call válida também não
contaminava o lote obrigatório.

O parser agora converte qualquer material de tool inválido em delta provisório,
sem reter ou imprimir o payload bruto. O normalizador aplica a decisão somente
no terminal Chat Completions: `stop` descarta tudo sem publicar ferramenta;
`tool_calls`/`function_call` exige lote integral válido e falha fechado se um
membro estiver contaminado. Placeholder anterior pode ser substituído por uma
call válida posterior. Codex/Responses e Anthropic mantêm validação estrita em
suas próprias fronteiras de protocolo.

### Evidência final

- 10 regressões novas RED→GREEN e uma borda adicional para irmão nulo;
- contratos focados: Chat Completions 17/17, adapters 26/26, Codex/Anthropic e
  providers ClinePass/OpenCode Go/Command Code verdes;
- `cargo fmt --all -- --check`, `cargo check --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings` e
  `git diff --check`: exit `0`;
- `cargo test --workspace -j 1 --no-fail-fast`: 82 suítes, **853 passed**,
  0 failed, 1 ignored (ConPTY físico), exit `0`;
- `refresh-slim.ps1 -Test`: exit `0`, imprimiu `OK:`;
- target/PATH: 13.049.856 bytes, SHA-256
  `2B5CE84DD60C55EEBEC70371C41468C658193C84EFE9A8ACA7C738BA3D092F2C`;
- `slim --version`: `slim 0.1.0`, exit `0`;
- companion local: HTTP 200 e mantido aberto;
- snapshot final do WIP: 200 entradas, 101 tracked e 99 untracked, sem reset,
  restore, checkout, clean, stash, pull, fetch ou provider real.

A passagem global posterior à correção não encontrou novo emissor precoce de
`MalformedToolCall` no wire OpenAI-compatible nem escrita diagnóstica desse
payload na TUI. O erro público continua possível apenas quando o provider exige
execução de ferramenta e entrega um lote realmente inválido; esse fail-closed é
intencional e impede side effects ambíguos.

## 13. Retomada autônoma — árvore de processos de Skills (G320)

### 13.1 Escopo e permissões

Retomada em 2026-08-27 sobre o checkout existente, sem reset, restore, checkout,
clean, stash, pull ou fetch. A investigação e os testes usaram somente o
filesystem local, processos locais, fixtures offline e loopback já presentes no
repositório. Nenhum provider real, credencial, mensagem externa ou rede de
produção foi usado. O WIP preexistente foi tratado como propriedade do usuário;
as mudanças ficaram limitadas ao bug confirmado, sua regressão e documentação
viva obrigatória.

### 13.2 Baseline reproduzível

- início: `2026-08-27T05:14:50-03:00`;
- branch `main`, `HEAD 25146cb8d001f99b188e06c7567f69c697a32ced`;
- WIP: 214 entradas (`101` tracked, `113` untracked, `1` deleted);
- diff tracked: 101 arquivos, 31.607 inserções e 3.565 remoções;
- nenhum processo `cargo`, `rustc` ou `slim` ativo;
- toolchain efetivo: Scoop MSVC, `rustc/cargo 1.97.1`;
- binários iniciais target/PATH: 13.830.144 bytes e SHA-256
  `219B878E74CFF35122B6264A9171119259E2932E69A5AE3031AA9DE3D813B950`;
- gate inicial: fmt/check/test/Clippy/diff-check verdes, 82 suítes,
  864 passed, 0 failed, 1 ConPTY físico ignored e 0 warnings.

O inventário coberto incluiu 189 arquivos Rust ativos/testes e 290 arquivos de
fonte, manifests, scripts, POCs e benches. `RULES.md`, `AGENTS.md`, manifests,
scripts de release/deploy e o contrato TUI foram lidos antes de alterar código.

### 13.3 Mapa do runtime auditado

1. argumentos → config em camadas → auth → adapter/provider;
2. prompt → agent loop → stream → projector → reducer/render TUI;
3. parse de tool → modo/capability → execução → captura bounded → contexto;
4. JSONL de sessão → preflight/recovery → resume/branch → compaction;
5. input TUI → `Action`/`Effect` → fila/evento → reducer → frame;
6. cancelamento/timeout → provider/subprocess/task → cleanup;
7. erro → tipo público → stderr/JSONL → exit code;
8. release → cópia estática no PATH → smoke → prova de identidade.

### 13.4 Estratégia de busca

Foram combinados rastreamento de fluxo, busca estrutural por `unwrap`/panic,
reads/coletas/canais/processos potencialmente unbounded, TODO/unsafe, erros
descartados, segredos/logs e duplicação de dependências. Cada candidato partiu
da hipótese nula de não-bug e só avançou após caminho alcançável, impacto e
reprodução determinística. Mudança de produção exigiu RED público antes do
menor GREEN.

### 13.5 Ciclos e correção confirmada

| Ciclo | Evidência RED | Correção mínima | GREEN |
|---|---|---|---|
| G320 — cancelamento de Skill | `cancelling_a_skill_terminates_its_process_tree` iniciou um descendente, cancelou o pai e observou o marcador do filho 1,5 s depois; falha: `a cancelled skill left a descendant process running` | cancelamento e timeout chamam `terminate_process_tree`; no Windows, `taskkill /PID <id> /T /F`, seguido do `kill` já existente como fallback, `wait` e drenagem dos pipes | regressão exata, arquivo `skill_invocation` inteiro (3/3), suíte workspace e gates finais verdes |

A correção está em `crates/slim-core/src/skills/invocation.rs`; a regressão está
em `crates/slim-core/tests/skill_invocation.rs`. Os diretórios temporários
criados pelo RED foram resolvidos para caminhos explícitos sob `%TEMP%`,
removidos e rechecados; nenhum `slim-skill-cancel-tree-*` permaneceu.

### 13.6 Hipóteses descartadas

| Candidato | Falsificação | Conclusão |
|---|---|---|
| output de Skill poderia crescer sem teto ou bloquear os readers | teste exploratório gerou 4 MiB com cap de 128 bytes e retornou `OutputTooLarge` em 0,55 s; o teste temporário foi removido e o hash original restaurado | não é bug |
| shell normal ainda capturaria stdout/stderr sem limite | caminho ativo usa captura raw limitada a 8 MiB e continua drenando os pipes | corrigido previamente; não reabrir |
| framers MCP/paste poderiam crescer no runtime de produção | não há caller de transporte MCP ativo; decoder bracketed examinado está restrito à fronteira testada | código dormente/contratual, sem impacto alcançável provado |
| recuperação de sessão alteraria arquivo implicitamente | preflight separa rejeição, `--recover` explícito e resume sem mutação nos casos inválidos | comportamento intencional e coberto |
| dependências duplicadas indicariam conflito executável | duplicações encontradas eram transitivas ou de desenvolvimento, sem falha de tipo/ABI reproduzível | sem achado |

### 13.7 Achados por prioridade desta retomada

| Prioridade | Confirmados | Estado |
|---|---:|---|
| P1 | 0 | nenhum |
| P2 | 1 | G320 resolvido e coberto por regressão |
| P3 | 0 | nenhum |
| P4 | 0 | nenhum |

### 13.8 Reauditoria após a última mudança de código

**Passagem A — global e mecânica:** `cargo fmt --all -- --check`,
`cargo check --workspace`, `cargo test --workspace -j 1 --no-fail-fast`,
`cargo clippy --workspace --all-targets -- -D warnings` e `git diff --check`:
todos exit 0. Resultado: 82 suítes, 865 passed, 0 failed, 1 ignored e
0 compiler warnings.

**Passagem B — adversarial e diferente:** 258 testes focados cobriram agent
loop, provider HTTP, abort/capabilities, durabilidade/JSONL/resume, Skills,
tools, auth/OAuth, TUI bridge e propriedades/fault/golden/PTY portável; todos
passaram. Benches release:

- TUI, 3.200 blocos / 4.968.694 bytes: scroll p95 0,996 ms e
  input→frame p95 1,228 ms, ambos abaixo de 16 ms;
- compaction selector: 10k p95 1,522 ms; 20k p95 2,914 ms; escala 1,92×.

As duas passagens consecutivas e distintas não produziram novo achado
confirmado após G320.

### 13.9 Gates, deploy e limitações objetivas

`refresh-slim.ps1 -Test` repetiu toda a suíte (865/0/1), compilou release,
copiou o executável estático e imprimiu `OK:`. Prova posterior:

- target e PATH: 13.830.144 bytes;
- SHA-256 idêntico:
  `DBFE58EE27FCE7D2F2001D88A7A4F0A6591F58A9AE911D0070A87B2FBD0008C3`;
- `Get-Command slim` → `C:\Users\User\bin\Slim.exe`;
- `slim --version` → `slim 0.1.0`, exit 0;
- build target/PATH: `2026-08-27T05:36:58.7160644-03:00`.

O teste físico ignorado foi executado isoladamente contra esse SHA. A primeira
tentativa foi inválida antes do teste pelo `RUSTC` de usuário inexistente; com o
toolchain Scoop fixado, o teste alcançou o EXE e falhou em `120x30-normal` por
`startup emitted no rendered SLIM frame`, `early_exit=None`. Portanto, a matriz
física de ConPTY/IME/mouse/clipboard continua não comprovada neste host; isso
não contradiz os testes portáveis nem foi classificado como bug. Integrações com
provider real e transporte MCP externo ficaram fora por escopo.

### 13.10 Estado final

O checkout terminou sem achado P1/P3/P4 e com o único P2 confirmado resolvido.
O WIP final permaneceu em 214 entradas (`101` tracked, `113` untracked,
`1` deleted); o diff tracked passou a 31.625 inserções pelas mudanças cirúrgicas
e documentação, preservando as 3.565 remoções preexistentes. Nenhuma operação
destrutiva de Git ou provider real foi usada. O critério de parada — duas
passagens globais diferentes e consecutivas sem novo achado após a última
mudança de código — foi satisfeito.
