# Slim Durable Harness v2 — Etapas 3–10

**Objetivo:** completar o harness durável sobre o contrato v2 já isolado, em fatias sequenciais verificáveis, preservando o writer v1 até o gate explícito de resume/rollout.

**Regras de execução:** cada etapa usa RED→GREEN focado, revisão independente e atualização do tracker. `restore` nunca executa efeitos. IDs, autorização, abort e replay são persistidos antes de qualquer integração de Skills/MCP/subagentes/Todo. Sem provider real nos testes; fixtures locais/offline. Sem dependências novas salvo RED incontornável e decisão documentada.

## Fronteiras congeladas

- `CURRENT_SCHEMA_VERSION == 1` e `SessionWriter` continuam como default; qualquer rollout v2 permanece opt-in e explícito.
- `JsonlRepo` v2 é opt-in; nenhuma conversão v1→v2 silenciosa.
- `ReplayPolicy::Never` nunca reexecuta; `Safe` apenas produz plano explícito enquanto não houver terminal confirmado.
- Reducer, restore, snapshot, watch e hooks observacionais não chamam provider, tool, skill, MCP ou spawn.
- Segredos, headers, credenciais, prompts e outputs brutos não entram em telemetria.
- Permanecem excluídos: multi-lane, SQLite/FTS, server/remote, leases, extension host público, Pi v3, WASM e background emulado.

## Etapa 3 — reducer e restore puros

- [x] Criar `session/reducer.rs` e `tests/session_reducer.rs`.
- [x] Reconstruir os quatro kinds com sequência crescente, gaps válidos e erro sem mutação.
- [x] Provar restore repetível, usage desconhecido preservado e JSONL byte-identical.
- [x] Revisão de spec/qualidade e gate focado com repos/recovery.

## Etapa 4 — Effects e durable run sem tools

- [x] Criar `session/effects.rs`, `session/manual_drive.rs` e `tests/session_durable_run.rs`.
- [x] Persistir user entry → operation/attempt → effect ID → resposta/usage/terminal.
- [x] Provar que restore de run incompleto não chama executor e não muda bytes.
- [x] Manter runtime/headless/TUI fora do wiring nesta etapa.

## Etapa 5 — retry, attempts e usage

- [x] Criar ledger puro de attempts e testes de correlação/ordem.
- [x] Preservar usage opcional; nunca converter desconhecido em zero.
- [x] Permitir retry apenas para falha de transporte segura e operação repetível.
- [x] Provar que retry mantém `operation_id` e não duplica operação terminal/não repetível.

## Etapa 6 — tool phases, replay e batches

- [x] Modelar intent/started/output/finished com IDs e batch estáveis.
- [x] Limitar batch/payload antes de persistir e rejeitar fases ambíguas.
- [x] Reconstruir batches e gerar replay plan puro para `Never`/`Safe`.
- [x] Provar que restore de batch concluído não reexecuta tool.

## Etapa 7 — queues e abort

- [x] Criar fila durável bounded/FIFO/deduplicada por `operation_id`.
- [x] Persistir claim, suspended, aborted e terminal sem perder prefixo confirmado.
- [x] Propagar cancelamento cooperativo nos limites de turno/provider/tool.
- [x] Provar que restore não recoloca automaticamente trabalho claimed/terminal.

## Etapa 8 — resume headless, depois TUI

- [x] Adicionar preflight/recovery explícito e `--resume` headless sem inferência automática.
- [x] Validar branch explícito, `next_seq`, códigos/mensagens e zero duplicação offline.
- [x] Manter sessão v1 rejeitada ou migrada apenas por comando explícito com backup.
- [x] Reusar o mesmo contrato na TUI somente após o headless GREEN, com histórico/redaction no mesmo handle, fence de eventos e `PendingRun`.

## Etapa 9 — snapshot, watch, hooks e telemetria

- [x] Snapshot imutável por prefixo/seq sem interromper append.
- [x] Watch cursorado sem duplicação e sem reparar bytes.
- [x] Hooks pós-append não autoritativos, com falha isolada.
- [x] Telemetria local bounded apenas com IDs/enums/contadores; teste-canário de segredo.

## Etapa 10 — Skills, MCP, subagentes e Todo/Plan/Goal

- [x] Compor catálogo único respeitando Auto/Plan/ReadOnly e autorização.
- [x] Persistir intent/terminal para Skills e MCP; restore não reexecuta.
- [x] Persistir parent/child, fila bounded, depth e cancelamento de subagentes.
- [x] Persistir operações tipadas de Todo/Plan/Goal e restaurar sem mutações duplicadas.
- [x] Fechar E2E offline root→skill/MCP→child→task→restore.

O bridge público do `Runtime` fecha o contrato durável offline. O registro
automático dessas capabilities no provider loop/CLI/TUI, transporte MCP
externo e processos reais de Skill/child permanecem fronteiras de adapter,
sem alegação de cobertura pelos testes desta etapa.

## Gates por etapa

1. RED focado registrado antes da implementação.
2. GREEN da nova suíte e regressões adjacentes.
3. Revisão independente sem P0/P1/P2 dentro do escopo.
4. Tracker/plano atualizados com limites reais.
5. `cargo test --workspace` e `refresh-slim.ps1 -Test` no fechamento da etapa.

## Gate final

- [x] Etapas 3–10 marcadas com evidência append-only.
- [x] Workspace verde, zero warnings; único ConPTY ignored explicitado.
- [x] Contagens canônicas atualizadas em todos os Markdown.
- [x] Binário do PATH implantado, smoke/version/hash registrados.
- [x] Limites, dívidas abertas e política de rollout v1/v2 explicitados.
