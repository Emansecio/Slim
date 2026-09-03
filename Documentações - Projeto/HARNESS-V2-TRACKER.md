# Slim Durable Harness v2 — tracker canônico

> Documento vivo do contrato de evolução do harness durável. A Onda 1 congela fronteiras, registra caracterização e entrega salvaguardas pequenas antes de ligar o v2. O `SessionWriter` v1 continua sendo o writer ativo durante toda a Onda 1.

## Estado de leitura rápida

- **Onda atual:** Etapas 2–10 concluídas; o gate canônico e o fechamento documental estão registrados abaixo.
- **Fronteira de produção:** o writer v1 continua ativo; o schema v2 ainda não está ligado ao caminho de produção.
- **Plano de execução:** [plano Slim Durable Harness v2 — Onda 1](../docs/superpowers/plans/2026-08-22-slim-harness-v2-wave-1.md).
- **Plano da Etapa 2:** [MemoryRepo, JsonlRepo e conformance](../docs/superpowers/plans/2026-08-22-slim-harness-v2-stage-2.md).
- **Plano das Etapas 3–10:** [reducer, restore e integração progressiva](../docs/superpowers/plans/2026-08-22-slim-harness-v2-stages-3-10.md).
- **Evidência da Task 1:** a linha documental inicial abaixo registra somente o baseline; aquela Task 1 não executou build nem testes. A Task 2 possui RED/GREEN real registrado em linha própria.
- **P2 deferred:** permanece fora das 10 etapas; não é uma etapa implícita nem um aceite desta onda.

## Onda 1 — guardrails antes do v2

- [x] **Documentação/plano:** tracker canônico criado e plano de implementação linkado ao trabalho da onda.
- [x] **Schema v2 não ligado:** contrato e inspeção v2 implementados/testados, sem troca do writer v1 no caminho de produção.
- [x] **Inspeção v1 read-only:** `inspect_session` lê somente a primeira linha, classifica v1/v2 e não cria quarentena nem altera bytes; teste público GREEN.
- [x] **Lock writer:** segundo writer é rejeitado enquanto o owner mantém o handle do sentinel aberto; caminho canônico impede bypass por alias e sessão/sentinel validam reparse points; o arquivo `.lock` persiste e a ownership é o handle vivo.
- [x] **Torn-tail repair:** cauda final incompleta é quarentenada e truncada antes do append; JSON inválido completo/interior retorna `InvalidData`.
- [x] **Teste `length` zero-tools:** fixture SSE offline com `write` válido e `finish_reason: "length"`; classificação interna do stop reason bruto ocorre antes da redaction, bloqueia toda execução e não emite `ToolStarted`, enquanto `AssistantEnded.reason` permanece redigido.
- [x] **Contrato de Effects apenas congelado:** a fronteira conceitual está registrada; a implementação continua na etapa 4.

## Etapas do harness

### 1. Schema, invariantes e caracterização

- [x] **Etapa 1 — congelar o contrato.**
- **Dependências:** baseline atual e fronteira explícita com o writer v1.
- **Entregáveis:** schema v2 de envelopes/records; invariantes de sequência, IDs, correlação e replay; inspeção v1/v2 somente leitura; caracterização do comportamento legado.
- **Checks de aceite:** registros de cada kind serializam com envelope monotônico; arquivos v1 são classificados sem mutação nem quarentena; schema v2 permanece fora do caminho de produção.
- **Estado atual factual:** contrato e inspeção v2 estão implementados e caracterizados; o schema v2 não está ligado e o writer v1 continua ativo.

### 2. MemoryRepo, JsonlRepo e conformance

- [x] **Etapa 2 — tornar os repositórios equivalentes sob o contrato.**
- **Dependências:** etapa 1 concluída.
- **Entregáveis:** `MemoryRepo`, `JsonlRepo` e suíte de conformance compartilhada para append, leitura, prefixo válido, ordenação e recuperação.
- **Checks de aceite:** a mesma sequência de casos passa nos dois repositórios; divergências de serialização, ordenação ou erro ficam explícitas; nenhuma troca prematura do writer v1 ocorre.
- **Estado atual factual:** concluída; `MemoryRepo` e `JsonlRepo` passam a mesma conformance de estado tipado, ordem, prefixo, gaps e rejeição sem mutação. Reopen, recuperação de bytes, quarantine, newline, schema inválido e lock permanecem cobertos na matriz específica do JSONL, incluindo o alvo canônico derivado do handle no Windows; v2 continua fora do caminho de produção.

### 3. Lock, reducer e restore sem efeitos

- [x] **Etapa 3 — garantir uma escrita e restauração pura.**
- **Dependências:** etapas 1 e 2.
- **Entregáveis:** lock de writer único; reparo de torn-tail; reducer determinístico; restore que reconstrói estado sem executar efeitos.
- **Checks de aceite:** segundo writer é rejeitado enquanto o owner existe; tail incompleto é quarentenado antes do append; reducer/restore são repetíveis e não emitem efeitos.
- **Estado atual factual:** concluída; reducer/restore puros reconstruem os quatro kinds com gaps válidos, preservam usage desconhecido, são repetíveis e não executam efeitos, com 11 testes focados. O writer v1 permanece ativo e o schema v2 continua desligado do caminho de produção.

### 4. Effects, manual drive e durable run sem tools

- [x] **Etapa 4 — separar estado durável de efeitos executáveis.**
- **Dependências:** etapa 3.
- **Entregáveis:** contrato de `Effects`; manual drive determinístico; execução durável sem tools; prova de que restore não reexecuta efeitos.
- **Checks de aceite:** um run sem tools pode ser persistido e restaurado; efeitos têm fronteira explícita e IDs de operação; replay de restore permanece sem side effects.
- **Estado atual factual:** concluída; Effects, manual drive determinístico e durable run sem tools persistem e restauram runs sem executar efeitos, com `session_durable_run` cobrindo 17 testes. Nenhum wiring em runtime, headless ou TUI foi iniciado; o writer v1 permanece ativo.

### 5. Retry e usage

- [x] **Etapa 5 — persistir tentativas e consumo.**
- **Dependências:** etapa 4.
- **Entregáveis:** modelo de retry/attempt; associação de usage por operação/tentativa; políticas de erro e repetição.
- **Checks de aceite:** tentativas são correlacionáveis e ordenadas; usage desconhecido permanece representável; retry não duplica operação não repetível.
- **Estado atual factual:** concluída; `RetryConfigured` é persistido, o ledger causal preserva attempts e correlação/ordenação, classes de erro são explícitas e usage unknown/partial permanece representável. `planned_provider_effects` permanece pending-only; `ManualDrive` inicial é nonretryable por design, com P2 de integração futura não bloqueante. O writer v1 permanece ativo.

### 6. Tool phases, replay e batches

- [x] **Etapa 6 — tornar tools observáveis e reproduzíveis.**
- **Dependências:** etapas 4 e 5.
- **Entregáveis:** fases de tool; replay policy; batches com correlação estável e limites explícitos.
- **Checks de aceite:** cada fase pode ser reconstruída; replay respeita `Never`/`Safe`; batches preservam ordem, IDs e resultados sem ambiguidade.
- **Estado atual factual:** concluída; invariantes de repo/reopen, limite máximo de 64 MiB, bounds de batch/payload, output obrigatório, legacy fail-closed e replay `Never`/`Safe` foram validados. A agregação mantém nota de custo O(n²); redaction permanece caller-trusted e artifact resolver fica para etapa futura. Nenhum wiring de produção foi ligado.

### 7. Queues e abort

- [x] **Etapa 7 — controlar concorrência e cancelamento.**
- **Dependências:** etapas 5 e 6.
- **Entregáveis:** filas duráveis; abort cooperativo; estados terminais e retomáveis.
- **Checks de aceite:** abort não perde o prefixo confirmado; filas não duplicam trabalho após restore; estados de abort são distinguíveis de erro e sucesso.
- **Estado atual factual:** concluída; queue bounded/FIFO/deduplicada cobre `Entry` e occupancy do repo, lifecycle de repo composto, e cancelamento seguro em provider/compaction/tool com stop reasons seguros. Cancel cooperativo não faz rollback; cancelamento durável e wiring ficam deliberadamente na Etapa 8.

### 8. Resume headless, depois integração TUI

- [x] **Etapa 8 — expor retomada headless e só então integrá-la à TUI.**
- **Dependências:** etapas 3 a 7; a integração TUI depende do resume headless validado.
- **Entregáveis:** seleção de sessão/recovery; resume headless; mensagens e códigos de saída estáveis; integração TUI posterior sobre o mesmo contrato.
- **Checks de aceite:** uma sessão pode ser retomada headless a partir do prefixo válido; recovery e branch são explícitos; nenhuma retomada é inferida de tail inválido.
- **Estado atual factual:** concluída; preflight/recovery explícitos, resume headless/TUI sobre o mesmo contrato, histórico/redaction no mesmo handle, fence de eventos, `PendingRun`, stress de 1.200 deltas e cancelamento derivado do resultado durável.

### 9. Snapshot, watch, hooks e telemetria

- [x] **Etapa 9 — observar o runtime sem alterar sua autoridade.**
- **Dependências:** etapas 3 a 8.
- **Entregáveis:** snapshots consistentes; watch; hooks; telemetria de operações, usage e falhas.
- **Checks de aceite:** snapshot não interrompe append; watch detecta mudanças sem duplicar eventos; hooks e telemetria não viram efeitos implícitos nem vazam segredos.
- **Estado atual factual:** concluída; snapshot/watch/hooks/telemetria bounded, leituras e linhas limitadas a 64 MiB, IDs sanitizados e falhas de observabilidade isoladas. Limite documentado: overwrite same-inode/same-length hostil não é detectado sem hash de prefixo.

### 10. Ligar Skills, MCP, subagentes e Todo

- [x] **Etapa 10 — conectar as superfícies de automação ao harness durável.**
- **Dependências:** etapas 4 a 9 e contratos de autorização das superfícies.
- **Entregáveis:** wiring de Skills/MCP; subagentes/child sessions; Todo/Plan/Goal; persistência e replay de suas operações.
- **Checks de aceite:** cada superfície usa o mesmo contrato durável; autorização e abort são preservados; operações podem ser auditadas e restauradas sem efeitos duplicados.
- **Estado atual factual:** concluída; catálogo/serviço durável e `RuntimeCapabilityBridge` compõem Skill, MCP selecionado, child scheduler/cancelamento/FIFO/reopen e operações tipadas Todo/Plan/Goal. O E2E offline root→Skill→MCP local selecionado→child→task→reopen não reexecuta efeitos. Limites: nenhuma superfície é registrada automaticamente no provider loop/CLI/TUI; não há transporte MCP externo, processo Skill/child real ou provider live no E2E.

## Exclusões desta linha de trabalho

Ficam fora das 10 etapas e não devem aparecer como requisitos implícitos: multi-lane; SQLite/FTS; server/remote; leases; public extension host; Pi v3; WASM; background emulado.

## Evidência append-only

As linhas abaixo só recebem acréscimos. Uma linha registra o comando ou a origem, o resultado observado e a etapa relacionada; números não são herdados de documentação antiga.

| Data | Comando/origem | Resultado observado | Etapa |
|---|---|---|---|
| 2026-08-22 | `cargo test -p slim-core --test session_recovery --test session_branch --test provider_fake --test agent_loop` | `EXIT=0; 17 passed` — baseline confirmado pela principal; não reexecutado nesta Task 1 | Baseline das etapas 1–4 |
| 2026-08-22 | `cargo test -p slim-core --test session_schema_v2` (RED `EXIT=101` → GREEN `EXIT=0`) e `cargo test -p slim-core` (`EXIT=0`) | Contrato público v2, quatro envelopes com `seq`, IDs/correlação/replay/usage opcional e inspeção v1/v2 read-only validados; writer v1 não ligado | Etapa 1 — contrato e caracterização |
| 2026-08-22 | `cargo test -p slim-core --test session_schema_v2` (`RUSTFMT_EXIT=0; FOCUSED_EXIT=0; 3 passed`) | `DurableEntry` prova árvore root/child (`parent_entry_id`) e correlação obrigatória de operação/tool (`operation_id`/`tool_call_id`); writer v1 continua sem wiring | Onda 1 — Task 2; Etapa 1 |
| 2026-08-22 | `cargo test -p slim-core --test session_recovery` (RED `EXIT=101`, 2 passed/2 failed → GREEN `EXIT=0`, 4 passed) | `competing_writer_is_rejected_until_owner_drops` e `reopening_after_torn_tail_repairs_before_append` reproduzem as falhas antes da correção e passam após lock sibling, parser de prefixo, quarantine e truncamento; `session_branch` adjacente GREEN (`EXIT=0`, 1 passed) | Onda 1 — Task 3; Etapa 3 (subchecks lock/torn-tail) |
| 2026-08-22 | `cargo test -p slim-core --test session_recovery` (RED `EXIT=101`, 4 falhas incluindo fixture sem parent → GREEN `EXIT=0`, 9 passed) e `cargo test -p slim-core --test session_branch` (GREEN `EXIT=0`, 1 passed) | Hardening da Task 3 validado: EOF válido sem newline recebe separador antes do append; quarantine usa criação atômica collision-safe; `recover` compartilha o lock; schema v1 é obrigatório; JSON inválido completo/interior falha sem quarantine | Onda 1 — Task 3; regressões de lock/recovery/torn-tail |
| 2026-08-22 | `cargo test -p slim-core --test agent_loop length_stop_blocks_all_tool_side_effects` (RED `EXIT=101`, assert de side effect) → `cargo test -p slim-core --test agent_loop` (GREEN `EXIT=0`, 14 passed) | Fixture SSE OpenAI-compatible local com `write` válido e `finish_reason: "length"`; sinal interno classifica o stop bruto antes da redaction e limpa as calls antes de tool-limit/execução, enquanto `AssistantEnded.reason` continua redigido; regressão com segredo exato `length` prova `[REDACTED]` sem side effect, e o caminho público `run_provider_turn` com `  MaX_ToKeNs  ` confirma nenhum `ToolStarted` | Onda 1 — Task 4; guardrail pré-Etapa 6 |
| 2026-08-22 | `cargo test -p slim-core --test session_recovery` (RED `EXIT=101` → GREEN `EXIT=0`, 11 passed) | Revisão final corrigida: somente EOF sintático é torn-tail; JSON completo/schema-inválido falha fechado; aliases usam alvo canônico e symlinks/reparse do sentinel não redirecionam o lock; dois testes Windows reais de symlink GREEN | Onda 1 — Task 3; fechamento dos P1 |
| 2026-08-22 | `rustfmt` focado + `cargo test -p slim-core --test session_schema_v2 --test session_recovery --test session_branch --test provider_fake --test agent_loop` | `RUSTFMT_EXIT=0; FOCUSED_EXIT=0; 31 passed / 0 failed` | Onda 1 — gate focado final |
| 2026-08-22 | `cargo test --workspace` | gate integral histórico da Onda 1; contagem supersedida pelo gate da Etapa 2 abaixo | Onda 1 — gate integral |
| 2026-08-22 | revisão final somente leitura + `.\refresh-slim.ps1 -Test` + `Slim.exe --version` | revisão histórica sem P0/P1 residual; deploy `OK:` e smoke aprovados; artefato supersedido pelo deploy da Etapa 2 abaixo | Onda 1 — concluída e implantada |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2` | `EXIT=0; 37 passed / 0 failed`; `MemoryRepo` e `JsonlRepo` passam a conformance comum; recuperação JSONL, lock e alvo canônico derivado do handle no Windows permanecem cobertos; v2 não ligado ao runtime/writer de produção | Etapa 2 — gate focado |
| 2026-08-22 | revisão final somente leitura | APROVADA; nenhum P0/P1/P2 residual; H2-B1 permanece aberto e fora do escopo da Etapa 2; `CURRENT_SCHEMA_VERSION` e writer v1 preservados | Etapa 2 — revisão |
| 2026-08-22 | `cargo test --workspace` | gate integral histórico da Etapa 2; contagem supersedida pelo gate da Etapa 3 abaixo | Etapa 2 — gate integral |
| 2026-08-22 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | deploy histórico da Etapa 2; artefato supersedido pelo deploy da Etapa 3 abaixo | Etapa 2 — concluída e implantada |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_reducer --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2` | `EXIT=0; 43 passed / 0 failed`; reducer/restore puro, quatro kinds, gaps, usage desconhecido, repetibilidade e ausência de efeitos validados junto das regressões das etapas 1–2; writer v1 permanece ativo | Etapa 3 — gate focado |
| 2026-08-22 | revisões independentes de especificação e qualidade | APROVADAS; nenhum P0/P1/P2 residual; restore não executa efeitos e nenhum wiring de runtime/CLI/TUI foi ligado | Etapa 3 — revisão |
| 2026-08-22 | `cargo test --workspace` | gate integral histórico da Etapa 3; contagem supersedida pelo gate da Etapa 4 abaixo | Etapa 3 — gate integral |
| 2026-08-22 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | deploy histórico da Etapa 3; artefato supersedido pelo deploy da Etapa 4 abaixo | Etapa 3 — concluída e implantada |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_durable_run --test session_reducer --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2` | `EXIT=0; 60 passed / 0 failed`; durable run sem tools e regressões das etapas 1–3 GREEN | Etapa 4 — gate focado |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_durable_run` | `EXIT=0; 17 passed / 0 failed`; persistência/restauração sem executor e sem alteração de bytes | Etapa 4 — suíte própria |
| 2026-08-22 | revisões independentes de especificação e qualidade | spec APROVADO sem P0/P1/P2; quality APROVADO sem P0/P1; P2 causal order de `planned_provider_effects` deferido obrigatoriamente às Etapas 5–6 antes de replay/wiring | Etapa 4 — revisão |
| 2026-08-22 | `cargo test --workspace` | gate integral histórico da Etapa 4; contagem supersedida pelo gate da Etapa 5 abaixo | Etapa 4 — gate integral |
| 2026-08-22 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | deploy histórico da Etapa 4; artefato supersedido pelo deploy da Etapa 5 abaixo | Etapa 4 — concluída e implantada |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_attempts --test session_durable_run --test session_reducer --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2` | `EXIT=0; 73 passed / 0 failed`; `session_attempts` cobre 11 testes; `RetryConfigured` persistido, ledger causal, error class, usage unknown/partial e `planned_provider_effects` pending-only validados | Etapa 5 — gate focado |
| 2026-08-22 | reexecução da matriz após uma execução com `session_recovery` NotFound | execução exata GREEN e matriz completa GREEN; NotFound classificado como flake transitório não confirmado, sem patch | Etapa 5 — estabilidade |
| 2026-08-22 | revisões independentes de especificação e qualidade | APROVADAS sem P0/P1/P2; `ManualDrive` inicial nonretryable por design; P2 de integração futura não bloqueante | Etapa 5 — revisão |
| 2026-08-22 | `cargo test --workspace` | gate integral histórico da Etapa 5; contagem supersedida pelo gate da Etapa 6 abaixo | Etapa 5 — gate integral |
| 2026-08-22 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | deploy histórico da Etapa 5; artefato supersedido pelo deploy da Etapa 6 abaixo | Etapa 5 — concluída e implantada |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_tool_phases --test session_attempts --test session_durable_run --test session_reducer --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2` | `EXIT=0; 89 passed / 0 failed`; `session_tool_phases` cobre 16 testes; invariantes repo/reopen, 64 MiB, bounds, output obrigatório, legacy fail-closed e replay validados | Etapa 6 — gate focado |
| 2026-08-22 | revisões independentes de especificação e qualidade | APROVADAS sem P0/P1/P2; nota O(n²) agregado, redaction caller-trusted e artifact resolver futuro registrados como limites | Etapa 6 — revisão |
| 2026-08-22 | `cargo test --workspace` | gate integral histórico da Etapa 6; contagem supersedida pelo gate da Etapa 7 abaixo | Etapa 6 — gate integral |
| 2026-08-22 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | deploy histórico da Etapa 6; artefato supersedido pelo deploy da Etapa 7 abaixo | Etapa 6 — concluída e implantada |
| 2026-08-22 | `cargo test -p slim-core --lib --test session_queue --test session_tool_phases --test session_attempts --test session_durable_run --test session_reducer --test session_repo_conformance --test session_jsonl_repo --test session_recovery --test session_branch --test session_schema_v2` | focused principal inicial `104 passed`; final `slim-core` `205 passed`; queue bounded/FIFO/dedup, `Entry`/occupancy, lifecycle repo composto, cancel provider/compaction/tool e stop reasons seguros GREEN | Etapa 7 — gate focado |
| 2026-08-22 | revisão independente de especificação e qualidade | spec APROVADO P0/P1/P2=0; quality P0/P1=0, apenas P2 de cobertura/cooperatividade; cancel cooperativo não faz rollback e cancelamento durável/wiring ficam na Etapa 8 | Etapa 7 — revisão |
| 2026-08-22 | `cargo test --workspace` | `EXIT=0; 54 suítes / 339 passed / 0 failed / 1 ConPTY ignored / 0 warnings` | Etapa 7 — gate integral |
| 2026-08-22 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | `OK:`; smoke `slim 0.1.0`, `EXIT=0`; SHA-256 `6642fd621ce9dfef5b93f291151fb1c80ac0ba3ea4343b1d1e89d0c863f05829`; 6.742.528 B; last write `2026-08-22 23:00:21` | Etapa 7 — concluída e implantada |
| 2026-08-23 | `cargo test -p slim-cli --test headless_resume --test tui_bridge --test tui_runtime --no-fail-fast` + `cargo test -p slim-core --test session_resume --test session_durable_run` + `cargo test -p slim-tui --lib` + `cargo test -p slim-cli --lib` | Etapa 8 GREEN: headless/TUI resume/recovery no mesmo contrato, histórico/redaction, fence/`PendingRun`, cancelamento verdadeiro e stress de 1.200 deltas; focused suites `headless_resume` 8, `tui_bridge` 10, `tui_runtime` 2, `session_resume` 9, `session_durable_run` 21, `slim-tui` 29 e `slim-cli` 10 | Etapa 8 — concluída |
| 2026-08-23 | `cargo test -p slim-core --test session_observability` | `EXIT=0; 7 passed / 0 failed`; snapshot/watch/hooks/telemetria bounded, cap de 64 MiB, IDs sanitizados e trust boundary same-inode/same-length registrados | Etapa 9 — concluída |
| 2026-08-23 | focused Stage10: `session_capabilities`, `mcp_contract`, `mode_capabilities`, `subagent_contract`, `runtime_abort`, `runtime_capabilities`, `skill_invocation` | `EXIT=0; 38 passed / 0 failed`; `session_capabilities` 19, `runtime_capabilities` 5; catálogo/serviço durável, autorização, claim/retry/Never, idempotência, child scheduler/cancel/FIFO/reopen, tarefas tipadas e E2E offline sem replay | Etapa 10 — concluída; revisões finais sem P0/P1/P2 dentro da aceitação |
| 2026-08-23 | `cargo test --workspace -j 1 --no-fail-fast` | `EXIT=0; 61 suítes / 407 passed / 0 failed / 1 ignored`; único `#[ignore]`: ConPTY físico em `crates/slim-cli/tests/tui_pty.rs:54`; 0 compiler warnings | Gate canônico final |
| 2026-08-23 | `cargo check --workspace`; `cargo fmt --all -- --check`; `git diff --check` | `EXIT=0` em todos os checks | Gate canônico final |
| 2026-08-23 | `refresh-slim.ps1 -Test` + `Slim.exe --version` | `OK:`; `Slim slim 0.1.0` em `C:\Users\User\bin\Slim.exe`, build `2026-08-23 00:25:22`, SHA-256 `8c6832647329909a4ffda3ad99585d746d53dc5aff4f93b9705dcb0cd3b376ff`, 7.513.088 B; smoke `EXIT=0` | Gate/deploy final; `CURRENT_SCHEMA_VERSION=1` e writer v1 default preservados |
