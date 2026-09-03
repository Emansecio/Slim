# Auditoria Estática — Concorrência, Runtime e Sessão/Estado (Slim Agent)

**Escopo:** `crates/slim-core/src/runtime/`, `crates/slim-core/src/session/`, `crates/slim-core/src/agents/`, `crates/slim-lsp/src/`
**Modo:** somente leitura (nenhum arquivo foi modificado)
**Data:** 2026-08-30
**Total de achados:** 32

Todos os trechos abaixo foram lidos diretamente dos arquivos com `Read`/`Grep`. Onde há dúvida sobre a
possibilidade real de disparo, isso está explicitamente marcado e a severidade foi reduzida.

---

## Sumário por severidade

| # | Arquivo:linha | Categoria | Severidade | Título resumido |
|---|---|---|---|---|
| 1 | `slim-core/src/model.rs:101-142` | bug | **crítica** | `send_interruptible` bloqueia thread async indefinidamente (risco de deadlock) |
| 2 | `slim-core/src/runtime/mod.rs:81-87` | bug | **crítica** | `CancellationToken::cancelled()` perde wakeup (falta `.enable()`) |
| 3 | `slim-core/src/model.rs:84,104,135,154,165,179,188,193` | bug | **alta** | Envenenamento de lock tratado com `.expect()` → pânico em cascata |
| 4 | `slim-core/src/runtime/governor.rs:143-248,546-572` + `runtime/mod.rs:1986,2018` | performance | **alta** | I/O de disco síncrono (até 32 MiB / 256 diretórios) em contexto async |
| 5 | `slim-core/src/runtime/mod.rs:2639-2647` + `session/capabilities.rs:1334-1355` | bug | **alta** | `todo` com array: revisão calculada uma vez → `RevisionConflict` no 2º item |
| 6 | `slim-core/src/interaction.rs:237,283-291` | bug | **alta** | `completed` nunca é limpo → `ask_question` quebra após 64 usos |
| 7 | `slim-core/src/session/queue.rs:333-367` | performance | **alta** | `enqueue_persisted` reconstrói a fila inteira a cada enqueue → O(n²) |
| 8 | `slim-core/src/session/repository.rs:172-192` | performance | **alta** | `prepare_batch` clona o estado derivado completo a cada append → O(n²) |
| 9 | `slim-lsp/src/manager.rs:281-297,340-345,716-735,1190-1243` | performance | **alta** | 2 leituras síncronas completas de arquivo + lock por resultado |
| 10 | `slim-core/src/model.rs:339-344,390,409` | performance | **alta** | `AppHandle.events` nunca drenado durante o loop → memória ilimitada |
| 11 | `slim-core/src/session/event_log.rs:141-148` | segurança | **alta** | `acquire_lock` não faz lock real (Unix) → TOCTOU entre processos |
| 12 | `slim-core/src/runtime/mod.rs:1473-1489` + vários `?` | bug | **alta** | Task de compactação em background vazada em caminhos de erro |
| 13 | `slim-lsp/src/pool.rs:279-298` | bug | **média** | `Lease::drop` fora de runtime → servidor nunca desligado (leak) |
| 14 | `slim-lsp/src/pool.rs:69-81` + `slim-lsp/src/process.rs:88-99` | bug | **média** | `Drop` executa `taskkill`/`kill` síncrono; sem `Drop for LspProcessPool` |
| 15 | `slim-lsp/src/pool.rs:369-374` | bug | **média** | `max_servers` contabiliza entradas idle → `ServerLimit` falso |
| 16 | `slim-lsp/src/manager.rs:228-234,262,1220` | segurança | **média** | TOCTOU `metadata()` → `read_to_string()`; symlink/OOM |
| 17 | `slim-core/src/runtime/mod.rs:2811` | performance | **média** | Canal de progresso `unbounded_channel` → memória ilimitada |
| 18 | `slim-core/src/runtime/governor.rs:530-544` | bug | **média** | `store_dependency` descarta silenciosamente ao atingir 256 |
| 19 | `slim-core/src/runtime/governor.rs:492-499` + `546-562` | performance | **média** | `evidence` ilimitada; `refresh_observed` não contabiliza diretórios |
| 20 | `slim-core/src/runtime/governor.rs:547-552` | performance | **média** | `refresh_observed` clona todo o `BTreeMap` a cada chamada |
| 21 | `slim-core/src/session/jsonl_repo.rs:273-335` + `capabilities.rs:1659-1666` | performance | **média** | `parse` lê arquivo inteiro + duplo parse; `restore_records` clona records |
| 22 | `slim-core/src/session/capabilities.rs:610-621,1369` | performance | **média** | `capabilities`/`children`/`task_idempotency` nunca podados |
| 23 | `slim-core/src/session/attempts.rs:679-684` | bug | **média** | Indexação `[]` em mapa/Vec sem verificação → pânico |
| 24 | `slim-core/src/runtime/mod.rs:2934,2961` | bug | **média** | `mem::replace(&mut self.app, ...)` sem guarda de pânico |
| 25 | `slim-core/src/runtime/mod.rs:3848-3854` | performance | **média** | `redact_values` faz N cópias completas do input |
| 26 | `slim-core/src/session/event_log.rs:215-234,236-249` | segurança | **média** | TOCTOU em `resolve_session_path` / `open_checked` |
| 27 | `slim-core/src/session/queue.rs:189-195,376-386` | bug | **média** | `claim_next` descarta id silenciosamente → dessincronia → pânico |
| 28 | `slim-lsp/src/transport.rs:395-410` | performance | **média** | Leitura do header byte-a-byte (1 syscall por byte) |
| 29 | `slim-lsp/src/instance.rs:342,399,479` | performance | **média** | `document_sync` mantido durante `notify().await` (escrita no servidor) |
| 30 | `slim-lsp/src/diagnostics.rs:45-49` | bug | **baixa** | Evicção arbitrária de URI (não é LRU como documentado) |
| 31 | `slim-core/src/session/capabilities.rs:938,964,966,1013,1033,1081,1126,1148` | estilo | **baixa** | `.expect()` em caminhos de produção (8 ocorrências) |
| 32 | `slim-core/src/session/capabilities.rs:1647-1649` | performance | **baixa** | Serializa o fact só para medir tamanho, e serializa de novo no append |

### Verificação negativa (sem achado)

- **`slim-lsp/src/path_policy.rs`** — **correto**. `existing_workspace_path` canonicaliza raiz *e* candidato e usa
  `Path::starts_with` (comparação por componente, não por prefixo de string), o que rejeita `..`, grafias
  alternativas e escapes por symlink/junction. Não há path traversal aqui. O risco real está no
  re-abrir por caminho *depois* da checagem (achado #16).
- **`std::sync::Mutex` mantido através de `.await`** — não encontrado. `transport.rs:103-107`,
  `runtime/mod.rs:3627-3630` e `3652-3655`, e `context/compact.rs:274-287` usam o padrão correto
  `unwrap_or_else(|e| e.into_inner())` e soltam o guard antes de qualquer await.
- **Ordenação de locks com ciclo** — não encontrada. Em `slim-lsp` a ordem é sempre
  `document_sync → state`; o drain task pega apenas `state`.
- **`unreachable!()` / `todo!()` / `unimplemented!()`** — nenhum nos arquivos do escopo.
- **Estouro/ truncamento em `as`** — `wire_version` (`instance.rs:670-672`), `lsp_document_identifier`
  (`document.rs:184`) e `branch_v2.rs:214` usam `clamp`/`min` antes do cast. Corretos.

---

# ACHADOS

---

## 1. `send_interruptible` bloqueia a thread async indefinidamente

- **Arquivo:** `D:\Slim\crates\slim-core\src\model.rs`
- **Linhas:** 101-142 (chamado de `model.rs:394`, alcançado via `runtime/mod.rs:3530-3541`)

```rust
    fn send_interruptible(&self, event: SessionEvent) -> SendOutcome {
        let mut event = Some(event);
        loop {
            let mut state = self.queue.state.lock().expect("event queue lock");
            let pending = event.as_ref().expect("pending event");
            if !state.receiver_alive {
                return SendOutcome::Disconnected;
            }
            if try_coalesce_tail(&mut state.events, pending) {
                state.coalesced_events = state.coalesced_events.saturating_add(1);
                return SendOutcome::Sent;
            }
            if state.events.len() < state.capacity {
                state.events.push_back(event.take().expect("pending event"));
                state.high_watermark = state.high_watermark.max(state.events.len());
                self.queue.not_empty.notify_one();
                return SendOutcome::Sent;
            }
            if self.cancellation.is_cancelled() {
                if is_cancel_droppable(pending) {
                    return SendOutcome::DroppedAfterCancellation;
                }
                if let Some(index) = state.events.iter().position(is_cancel_droppable) {
                    state.events.remove(index);
                    state.events.push_back(event.take().expect("pending event"));
                    self.queue.not_empty.notify_one();
                    return SendOutcome::Sent;
                }
            }
            let wait_started = Instant::now();
            let (mut next, _) = self
                .queue
                .not_full
                .wait_timeout(state, Duration::from_millis(1))
                .expect("event queue wait");
```

- **Categoria:** bug
- **Severidade:** **crítica**

**Descrição:** este é um `Condvar::wait_timeout` em *loop infinito* dentro de um `std::sync::Mutex`. Ele é
alcançado a partir de `AppHandle::push_event` (`model.rs:394`) → `push_runtime_event`
(`runtime/mod.rs:3530`), que é chamado de dentro de **todas** as funções `async` do loop do agente:
`execute_provider_tool_batch`, `execute_code_intel`, `execute_ask_question`, `materialize_results`,
`compact_before_send`, `cancel_pending_background` e `ProviderStreamNormalizer::push_inner`
(um evento por delta de streaming).

Consequências concretas:
1. Se a fila limitada estiver cheia e o consumidor (TUI / headless) parar de drenar, a **thread worker do
   runtime tokio fica presa** nesse loop — não é apenas lento, é bloqueio de thread com mutex de SO.
2. **Auto-deadlock:** se o consumidor for ele próprio uma task no mesmo runtime tokio (é o caso do bridge
   de eventos da TUI), e houver menos workers livres do que produtores bloqueados, o consumidor **nunca
   é escalonado**, logo nunca drena, logo os produtores nunca saem do loop. Deadlock permanente.
3. A única saída é `is_cancel_droppable` (`model.rs:308-320`), que cobre **apenas**
   `AssistantTextDelta`, `ReasoningDelta`, `ToolOutput` e `ToolProgress`. `ToolStarted`, `ToolFinished`,
   `ContextSnapshot`, `Usage`, `CompactionState`, `ArtifactStored` etc. **nunca** são descartáveis — ou
   seja, mesmo com cancelamento solicitado esses eventos continuam bloqueando.
4. Além disso, `is_cancelled()` só é consultado **depois** do teste de capacidade, então o caminho de
   cancelamento depende de o evento ser descartável.

**Correção sugerida:** nunca bloquear dentro do runtime async. Duas opções:
(a) tornar o sender assíncrono (`tokio::sync::mpsc` ou `tokio::sync::Notify`) e propagar o `.await` para
`push_runtime_event`, transformando-o em `async fn`; ou
(b) manter o canal síncrono, mas trocar `send_interruptible` por `try_send` (já existe em
`model.rs:83`) em todos os caminhos async, descartando o evento quando a fila estiver cheia — a
filha já é tratada como "best effort" pelo `push_transient_event` (`model.rs:401-417`), então perder
um delta de streaming é aceitável e preferível a travar o processo.
Como medida imediata de contenção, adicione um limite total de espera (ex.: 50 ms) e, ao estourar,
retorne `SendOutcome::DroppedAfterCancellation` para **qualquer** tipo de evento.

---

## 2. `CancellationToken::cancelled()` perde o wakeup (notificação perdida)

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\mod.rs`
- **Linhas:** 81-87

```rust
    pub async fn cancelled(&self) {
        let notified = self.0.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
```

- **Categoria:** bug
- **Severidade:** **crítica**

**Descrição:** `tokio::sync::Notify::notified()` só registra o waiter quando o future é **polado pela
primeira vez** (ou quando `enable()` é chamado explicitamente). Já `cancel()` usa `notify_waiters()`
(linha 73), que acorda **apenas os waiters já registrados** e **não armazena nenhuma permissão**.

Sequência que trava:
1. Task cria `notified` (ainda não registrado).
2. Task testa `is_cancelled()` → `false`.
3. Outra thread chama `cancel()` → grava a flag e chama `notify_waiters()`. **Não há waiter
   registrado, portanto a notificação é descartada.**
4. Task faz `notified.await` → registra-se → **dorme para sempre**, mesmo com a flag já em `true`.

Isso trava permanentemente qualquer `select!` que dependa desse future. Pontos de impacto verificados:
`execute_ask_question` (`runtime/mod.rs:2393-2396`), `compact_before_send` (`runtime/mod.rs:3150-3155`),
`run_background_compaction` (`runtime/mod.rs:3589-3597`) e `LspTransport::request_cancellable`
(`slim-lsp/src/transport.rs:636-656`).

A janela é estreita (entre a checagem e o primeiro poll), mas em runtime multi-thread `cancel()` é
chamado de fora (thread de sinal / thread da TUI / outro worker), então é perfeitamente alcançável.
O próprio código demonstra conhecer o padrão correto — `pool.rs:365-366`, `pool.rs:646-647` e
`transport.rs:281-282` chamam `notified.as_mut().enable()` **antes** de testar o estado.

**Correção sugerida:** registre o waiter antes de testar a flag:

```rust
pub async fn cancelled(&self) {
    let notified = self.0.notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if self.is_cancelled() {
        return;
    }
    notified.await;
}
```

Alternativa mais robusta: usar `notify_one()` (que armazena permissão) em vez de `notify_waiters()` em
`cancel()`, ou manter ambos.

---

## 3. Envenenamento de lock tratado com `.expect()` em vez de `into_inner()`

- **Arquivo:** `D:\Slim\crates\slim-core\src\model.rs`
- **Linhas:** 84, 104, 135, 154, 165, 179, 188, 193, 297

```rust
    pub fn try_send(&self, event: SessionEvent) -> Result<(), TrySendError<SessionEvent>> {
        let mut state = self.queue.state.lock().expect("event queue lock");
```
```rust
            let (mut next, _) = self
                .queue
                .not_full
                .wait_timeout(state, Duration::from_millis(1))
                .expect("event queue wait");
```
```rust
impl Drop for SessionEventSender {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().expect("event queue lock");
        state.sender_count = state.sender_count.saturating_sub(1);
```

- **Categoria:** bug
- **Severidade:** **alta**

**Descrição:** confirmado por busca: **não existe uma única ocorrência** de
`unwrap_or_else(|e| e.into_inner())` em todo o workspace. Todos os locks são destravados com
`.expect(...)`. Se **qualquer** pânico ocorrer enquanto o mutex está mantido — por exemplo dentro de
`try_coalesce_tail` (`model.rs:242-279`, que faz `push_str` e `clone_from`) ou em qualquer closure
passada a `with_state_mut` — o mutex fica envenenado. A partir daí, **todo** `push_event`, `try_send`,
`recv`, `stats` e os `Drop` de `SessionEventSender`/`SessionEventReceiver` panificam, derrubando o
processo inteiro. Como esse é o canal por onde passam todos os eventos de UI, um pânico localizado
vira falha total e irreversível.

O contraste dentro do próprio código mostra que o padrão correto é conhecido — `context/compact.rs:274-287`

```rust
    fn with_state<T>(&self, read: impl FnOnce(&CompactionHandleState) -> T) -> T {
        match self.0.lock() {
            Ok(state) => read(&state),
            Err(poisoned) => read(&poisoned.into_inner()),
        }
    }
```

e `runtime/mod.rs:3627-3630` / `3652-3655` também usam `poisoned.into_inner()`.

**Correção sugerida:** introduzir um helper e usá-lo em todos os pontos de `model.rs`:

```rust
fn lock_state(queue: &EventQueue) -> std::sync::MutexGuard<'_, EventQueueState> {
    queue.state.lock().unwrap_or_else(|e| e.into_inner())
}
```
Substituir `.expect("event queue wait")` por
`.unwrap_or_else(|e| e.into_inner())` (o `PoisonError<MutexGuard>` carrega o guard). Avaliar
`parking_lot::Mutex` (sem poisoning) para toda a base de código.

---

## 4. I/O de disco síncrono (até 32 MiB / 256 diretórios) dentro de funções `async`

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\governor.rs`
- **Linhas:** 143-248 (`observe_before`), 546-572 (`refresh_observed`), 751-833 (`snapshot_path` / `fingerprint_*`)
- **Ponto de chamada:** `D:\Slim\crates\slim-core\src\runtime\mod.rs:1986` e `2018`

```rust
        let dependency_before = match resolved_spec.dependency_scope {
            ToolDependencyScope::TargetFile => {
                match dependency_snapshot(&arguments_value, DependencyKind::File) {
                    Ok(snapshot) => {
                        dependency_changed = self.track_dependency(&snapshot);
                        Some(snapshot)
                    }
```
```rust
    fn refresh_observed(&mut self) -> Result<bool, ()> {
        let dependencies = self
            .ledger
            .observed
            .iter()
            .map(|(key, dependency)| (key.clone(), dependency.clone()))
            .collect::<Vec<_>>();
        let mut changed = false;
        let mut refreshed_bytes = 0u64;
        for (key, dependency) in dependencies {
            if matches!(dependency.kind, DependencyKind::File) {
                let bytes = fs::metadata(&dependency.path).map_or(0, |metadata| metadata.len());
                refreshed_bytes = refreshed_bytes.saturating_add(bytes);
                if refreshed_bytes > MAX_REFRESH_BYTES {
                    return Err(());
                }
            }
            let current = snapshot_path(dependency.kind, dependency.path.clone())?;
```
```rust
        let (pending, observations) = governor.observe_before(
            self.tools.operational_spec(&call.name),
            cwd,
            &call.name,
            &call.arguments,
        );
        self.emit_governor_observations(observations, &mut next_seq)?;
```

- **Categoria:** performance
- **Severidade:** **alta**

**Descrição:** `observe_before` executa `std::fs::File::read` + SHA-256 e `fs::read_dir` de forma
**completamente síncrona**. É chamado diretamente — sem `spawn_blocking` — de dentro de
`Runtime::execute_provider_tool_batch`, que é `async fn`:
- linha 1986: dentro do loop sequencial de ferramentas (uma chamada por ferramenta, em série);
- linha 2018: em loop para **todas** as ferramentas read-only do batch, **antes** de montar as futures.

Agregado: `MAX_REFRESH_BYTES = 32 MiB` (`governor.rs:19`) para `ObservedWorkspace`, `MAX_DEPENDENCY_BYTES = 8 MiB`
por arquivo, até `MAX_DIRECTORY_ENTRIES = 4096` entradas por diretório e até `MAX_OBSERVED_DEPENDENCIES = 256`
dependências re-hashadas. Ou seja, um único tool call pode fazer **dezenas de MiB de leitura síncrona** na
thread do runtime tokio, travando **todas** as outras tasks (incluindo o drain de eventos da TUI, o que
agrava o achado #1).

Observação adicional: o orçamento de bytes em `refresh_observed` só contabiliza
`DependencyKind::File` (linhas 556-562). `DependencyKind::Directory` entra em `snapshot_path` →
`fingerprint_directory` (`read_dir` + `metadata()` por entrada, até 4096) **sem nenhuma contabilização**,
de modo que o limite de 32 MiB não protege contra 256 `read_dir` em sequência.

**Correção sugerida:** envolver o governor em `tokio::task::spawn_blocking`, ou tornar
`observe_before`/`observe_after` `async` e mover `fingerprint_file`/`fingerprint_directory` para
`spawn_blocking`. Alternativamente, cachear os fingerprints por `(path, mtime, size, len)` para evitar
re-hashar conteúdo inalterado. E estender `refreshed_bytes` para contabilizar também diretórios
(ex.: `MAX_DIRECTORY_ENTRIES` por chamada), transformando o limite em um orçamento global real.

---

## 5. `todo` com array: revisão calculada uma vez → conflito de revisão no 2º item

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\mod.rs` (produtor) e `D:\Slim\crates\slim-core\src\session\capabilities.rs` (validador)
- **Linhas:** `runtime/mod.rs:2639-2647`; `capabilities.rs:1334-1355`

Produtor:
```rust
        let revision = bridge.task_revision("session").saturating_add(1);
        let mut lines = Vec::new();
        for (index, (label, mutation)) in mutations.into_iter().enumerate() {
            let request = TaskMutationRequest {
                idempotency_key: format!("todo-{}-{index}", std::process::id()),
                entity_id: "session".into(),
                revision,
                mutation,
            };
            match bridge.apply_task_mutation(request, mode, AuthorizationGrant::Explicit) {
                Ok(_changed) => lines.push(format!("todo updated: {label}")),
                Err(error) => {
                    return ToolResult {
                        name: invocation.name.into(),
                        success: false,
                        output: format!("todo rejected: {error}"),
                        artifact: None,
                    }
                }
            }
        }
```

Validador:
```rust
        let actual = self
            .tasks
            .get(&request.entity_id)
            .map(|task| task.revision)
            .unwrap_or(0);
        if self.task_mutations(&request.entity_id).len() >= MAX_TASK_COLLECTION {
            return Err(CapabilityLedgerError::InvalidIdentifier("task mutations"));
        }
        let expected = actual
            .checked_add(1)
            .ok_or(CapabilityLedgerError::RevisionConflict {
                entity_id: request.entity_id.clone(),
                expected: u64::MAX,
                actual,
            })?;
        if request.revision != expected {
            return Err(CapabilityLedgerError::RevisionConflict {
                entity_id: request.entity_id,
                expected,
                actual,
            });
        }
```

- **Categoria:** bug
- **Severidade:** **alta**

**Descrição:** clássico read-modify-write não atômico. `revision` é lido **uma única vez** antes do loop
(`runtime/mod.rs:2639`) e reutilizado para todas as N mutações. A 1ª mutação passa
(`expected == actual + 1`) e grava `tasks["session"].revision = revision` (`capabilities.rs:1379-1382`).
Na 2ª iteração, `actual` já é `revision`, logo `expected` passa a ser `revision + 1`, e o pedido com
`revision` é rejeitado com `RevisionConflict` → a tool `todo` retorna
`"todo rejected: revision conflict"` e aborta o lote inteiro.

Isso é alcançável pelo schema declarado: `todo_tool_definition()` (`runtime/mod.rs:3307-3335`) aceita
`{"todos": [ {title/content, id, status}, ... ]}` — justamente o caso multi-item — e o parser
(`runtime/mod.rs:2585-2607`) produz uma mutação por entrada.

Efeito colateral: a 1ª mutação já foi persistida de forma durável (`append_fact`, `capabilities.rs:1357`)
antes da falha, então o estado fica parcialmente aplicado e o agente recebe um erro enganoso.

**Correção sugerida:** recalcular a revisão a cada iteração, ou aplicar o lote atomicamente:

```rust
for (index, (label, mutation)) in mutations.into_iter().enumerate() {
    let revision = bridge.task_revision("session").saturating_add(1);
    let request = TaskMutationRequest {
        idempotency_key: format!("todo-{}-{index}", std::process::id()),
        entity_id: "session".into(),
        revision,
        mutation,
    };
    ...
}
```
Melhor ainda: acrescentar `apply_task_mutations(Vec<TaskMutationRequest>)` a `CapabilityService` para
validar e persistir todas as mutações em um único `append_batch`, com roll-back do estado em memória em
caso de falha. Adicionar também um teste que envie `{"todos":[...]}` com 2+ entradas.

---

## 6. `completed` nunca é limpo → `ask_question` para de funcionar após 64 usos

- **Arquivo:** `D:\Slim\crates\slim-core\src\interaction.rs`
- **Linhas:** 16, 237, 268, 283-291

```rust
const MAX_INTERACTIONS_PER_ROUTE: usize = 64;
```
```rust
        if state.pending.len().saturating_add(state.completed.len()) >= MAX_INTERACTIONS_PER_ROUTE {
            return Err(InteractionError::RouteCapacity);
        }
```
```rust
impl Drop for PendingQuestion {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if state.pending.remove(&self.request_id).is_some() {
                state.completed.insert(self.request_id.clone());
            }
        }
    }
}
```

- **Categoria:** bug
- **Severidade:** **alta**

**Descrição:** `completed` (`interaction.rs:155`) é um `HashSet` que só cresce: inserido em `268`
(resposta entregue) e em `287` (drop do `PendingQuestion`). Busca confirmada — **não há nenhum**
`completed.clear()` / `completed.remove()` no arquivo. Como `register()` soma
`pending.len() + completed.len()` contra `MAX_INTERACTIONS_PER_ROUTE = 64`, após 64 interações de
`ask_question` — **incluindo as canceladas e as que nunca foram respondidas**, pois `Drop` também
move para `completed` — toda chamada subsequente a `register()` retorna `RouteCapacity`.

Efeito visível: `execute_ask_question` (`runtime/mod.rs:2373-2379`) passa a devolver sempre
`"ask_question: route capacity exceeded"` (ou similar) e a ferramenta fica permanentemente inutilizável
pelo resto da vida do processo/sessão, sem nenhuma forma de recuperação.

**Correção sugerida:** (a) dar limite a `completed` (ex.: manter no máximo os últimos N, ou usar um
`VecDeque` com capacidade `MAX_INTERACTIONS_PER_ROUTE` descartando o mais antigo); ou (b) contar apenas
`pending.len()` contra o limite de capacidade, usando `completed` exclusivamente para detecção de
duplicidade (com o mesmo teto aplicado). E (c) não inserir em `completed` quando a pergunta foi
cancelada — só quando efetivamente respondida.

---

## 7. `enqueue_persisted` reconstrói a fila inteira a cada enqueue (O(n²))

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\queue.rs`
- **Linhas:** 333-367

```rust
    pub fn enqueue_persisted<R: DurableRepo>(
        &mut self,
        repo: &mut R,
        item: QueueItem,
    ) -> Result<(), DurableQueueError> {
        let restored = Self::from_repo(self.capacity, repo)?;
        if self.is_pristine() {
            *self = restored;
        } else if *self != restored {
            return Err(DurableQueueError::InvalidRecords(
                "in-memory queue state does not match durable repository prefix".into(),
            ));
        }
        self.validate_item(&item)?;
        self.ensure_not_seen(&item.operation_id)?;
        if repo
            .records()
            .iter()
            .any(|record| record_operation_id(record) == Some(item.operation_id.as_str()))
        {
```

- **Categoria:** performance
- **Severidade:** **alta**

**Descrição:** cada `enqueue_persisted` executa **três varreduras completas** sobre todos os registros
duráveis: (1) `from_repo` → `from_records` (`queue.rs:296-316`) re-aplica e re-valida todos os
`DurableRecord`; (2) `*self != restored` compara filas inteiras (mapas com todos os itens);
(3) `repo.records().iter().any(...)` procura duplicidade linearmente. Só depois disso o item é
efetivamente inserido.

Com n itens enfileirados, o custo total é O(n²) em tempo e o pico de memória é o dobro do estado da
fila (a cópia `restored` mais `self`). Numa sessão longa com milhares de operações duráveis isso
degrada rapidamente.

**Correção sugerida:** manter o validador incremental do repositório (`DurableAppendValidator`, já
existente em `repository.rs:102`) como a única autoridade, em vez de reconstruir a fila do zero. Para a
checagem de duplicidade, usar um `HashSet<&str>` de `operation_id` derivado dos registros (já existe
`self.operation_ids`) ou um índice auxiliar no repo, evitando a terceira varredura.

---

## 8. `prepare_batch` clona o estado derivado completo a cada append (O(n²))

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\repository.rs`
- **Linhas:** 172-196

```rust
    pub(crate) fn prepare_batch(
        &self,
        prefix: &[DurableRecord],
        records: &[DurableRecord],
    ) -> io::Result<PreparedDurableAppend> {
        let mut next = self.clone();
        for (index, record) in records.iter().enumerate() {
            if index == 0 {
                validate_next(prefix, record.seq())?;
            } else {
                validate_next(&records[..index], record.seq())?;
            }
            next.apply_one(
                prefix,
                &records[..index],
                record,
                io::ErrorKind::InvalidInput,
            )?;
        }
        Ok(PreparedDurableAppend { validator: next })
    }

    pub(crate) fn commit(&mut self, prepared: PreparedDurableAppend) {
        *self = prepared.validator;
    }
```

- **Categoria:** performance
- **Severidade:** **alta**

**Descrição:** para acrescentar **um** registro, o código clona o `DurableAppendValidator` inteiro
(`self.clone()`), que carrega: `reducer: DurableState` (estado reduzido completo da sessão),
`lifecycle: BTreeMap`, `attempts: AttemptLedger` (todas as tentativas + mapa de usage),
`queue: DurableQueue` (todas as entradas com payload) e `tools: ToolPhaseLedger`. Em seguida aplica um
registro e move o resultado de volta em `commit`.

O custo por append é O(tamanho do estado derivado); ao longo de n appends, O(n²) em tempo e allocations.
É chamado por `MemoryRepo::append`/`append_batch` (`memory_repo.rs:31-46`) e por
`JsonlRepo::append_batch` (`jsonl_repo.rs:220-262`) — ou seja, está no caminho de **todo** append
durável, inclusive pelo `CapabilityService::append_fact` (`capabilities.rs:1653`), que grava 2-3 facts
por dispatch de capability.

**Correção sugerida:** aplicar os registros **in-place** e desfazer em caso de erro, em vez de
clonar. O padrão seguro é calcular um *journal* de desfazer dentro de `apply_one` (guardas de
`Drop` por campo) ou expor `apply_one_reversible` que retorne as entradas anteriores. Se a semântica
"tudo ou nada" do lote for indispensável, clonar **uma vez por lote** (já é o caso em `prepare_batch`)
é aceitável; o problema real é que `MemoryRepo::append` chama `prepare` (singular) para cada registro
individual, forçando um clone por registro — trocar por `append_batch` com um único `prepare_batch`
já resolve a maior parte do custo.

---

## 9. `slim-lsp`: duas leituras síncronas completas de arquivo + lock por resultado

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\manager.rs`
- **Linhas:** 281-297 (`human_position`), 340-345 (`line_context`), 716-735 (`references`), 1190-1243 (`diagnostics`)

```rust
    async fn human_position(
        instance: &crate::instance::LspServerInstance,
        path: &Path,
        lsp_line: u32,
        lsp_character: u32,
    ) -> (u32, u32) {
        let encoding = instance.snapshot().await.encoding;
        if let Some(text) = Self::read_capped(path) {
            let lines = split_physical_lines(&text);
            if let Some((line, column, _byte)) =
                PositionCodec::lsp_to_human(encoding, &lines, lsp_line, lsp_character)
            {
                return (line, column);
            }
        }
        (lsp_line.saturating_add(1), lsp_character.saturating_add(1))
    }
```
```rust
fn line_context(path: &Path, line_index: u32) -> Option<String> {
    let text = LspCodeIntelligence::read_capped(path)?;
    let lines = split_physical_lines(&text);
    let raw = lines.get(line_index as usize)?;
    Some(truncate_text(raw, MAX_CONTEXT_LINE_CHARS))
}
```
```rust
            let (line, column) =
                Self::human_position(&instance, &path, target_line, target_char).await;
            by_file.entry(relative).or_default().push((
                line,
                column,
                line_context(&path, target_line),
            ));
```
```rust
                rows.push(Self::diagnostic_row(&instance, &path, item).await);
```

- **Categoria:** performance
- **Severidade:** **alta**

**Descrição:** `read_capped` (`manager.rs:228-234`) faz `std::fs::metadata` + `std::fs::read_to_string`
(até `MAX_CONTEXT_READ_BYTES = 4 MiB`) — **I/O síncrono** — e `split_physical_lines` aloca um `Vec<&str>`
com todas as linhas. Em `references` (linhas 716-735), para **cada** localização retornada pelo servidor:
- `human_position` → `instance.snapshot().await` (**adquire o mutex de estado**) + `read_capped` + split;
- `line_context` → `read_capped` **novamente no mesmo arquivo** + split.

Ou seja: **2 leituras completas de disco, 2 splits e 1 aquisição de mutex por resultado**
(até `MAX_CODE_INTEL_RESULTS`). No caminho de `diagnostics` de workspace (linhas 1190-1243) há ainda um
`read_capped` por arquivo (linha 1220) e um `diagnostic_row` → `human_position` por item (linha 1230).
Tudo isso rolando de forma síncrona numa thread do runtime tokio (o `code_intel` é chamado via
`run_code_intel_request`, `runtime/mod.rs:3239`).

**Correção sugerida:** (1) cachear `(path → texto+linhas)` por operação — um `HashMap<PathBuf, (String, Vec<Range>)>`
montado uma vez no início de `references`/`diagnostics` elimina as leituras repetidas; (2) ler o encoding
da instância **uma vez** antes do loop em vez de `instance.snapshot().await` por item; (3) ler apenas a
linha necessária (`BufReader` + `lines().nth()`) em vez do arquivo inteiro para `line_context`;
(4) executar as leituras via `tokio::task::spawn_blocking` + `tokio::fs`.

---

## 10. `AppHandle.events` nunca é drenado durante o loop → memória ilimitada

- **Arquivo:** `D:\Slim\crates\slim-core\src\model.rs`
- **Linhas:** 339-344, 390, 409 (`drain_events` em 419-421)
- **Consumidores:** `D:\Slim\crates\slim-core\src\runtime\mod.rs:1232`, `1278`, `1356`, `1567`, `1131`, `3712`

```rust
pub struct AppHandle {
    snapshot: Option<SessionSnapshot>,
    events: Vec<SessionEvent>,
    last_event_seq: Option<u64>,
    event_sender: Option<SessionEventSender>,
}
```
```rust
    pub fn push_event(&mut self, event: SessionEvent) -> Result<(), &'static str> {
        if self
            .last_event_seq
            .is_some_and(|last_seq| event.seq <= last_seq)
        {
            return Err("event sequence must increase");
        }
        self.last_event_seq = Some(event.seq);
        self.events.push(event.clone());
```
```rust
            let event_start = self.app.events().len();
            let request_next_seq = checked_next_seq(next_seq)?;
```

- **Categoria:** performance
- **Severidade:** **alta**

**Descrição:** `events` é um `Vec` sem limite. `push_event` acrescenta **todos** os eventos — inclusive
cada `AssistantTextDelta`, cada `ToolProgress` e cada `ToolOutput` (cada um até `max_result_bytes`).
O `Vec` **não pode ser drenado** porque o loop do agente depende dele como índice:
`let event_start = self.app.events().len();` (linha 1232) e `app.events().get(event_start..)`
(linhas 1356, 1567, 3565, 3712).

Busca confirmada: `drain_events()` só é chamado em `slim-cli/src/headless.rs:1124` — **após** o término
da execução. No modo TUI não é chamado nunca. Portanto, ao longo de uma execução longa (centenas de
turnos, milhares de tool calls), a memória cresce de forma ilimitada e monotonicamente, com pico
proporcional a `número_de_eventos × tamanho_médio_do_output`. Numa sessão com outputs de shell de
centenas de KB isso chega a GiB.

Agrava: `self.events.push(event.clone())` clona o evento (necessário porque `send_interruptible`
consome o original), dobrando o custo de alocação por evento.

**Correção sugerida:** não manter o histórico completo em memória. Como o loop só precisa de
"eventos a partir de `event_start` deste turno", usar um buffer circular limitado (ex.:
`VecDeque` com capacidade = janela do turno) e fazer com que `event_start` seja um **índice absoluto
monotônico** (`first_seq_index`) em vez de um índice de `Vec`. Assim `get(event_start..)` passa a ser
`range(first_seq_index - base ..)` sobre o ring buffer. Alternativamente, drenar e persistir
incrementalmente no `SessionWriter` a cada turno (já existe em `event_log.rs:99-127`) e manter apenas a
janela do turno corrente.

---

## 11. `acquire_lock` não adquire lock nenhum (Unix) → TOCTOU entre processos

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\event_log.rs`
- **Linhas:** 141-148 (usado em `event_log.rs:58` e `jsonl_repo.rs:48,139`)

```rust
pub(crate) fn acquire_lock(path: &Path) -> io::Result<File> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(windows)]
    options.share_mode(FILE_SHARE_READ);
    open_checked(options, &lock_path)
}
```

- **Categoria:** segurança
- **Severidade:** **alta**

**Descrição:** apesar do nome, a função apenas **abre/cria** um arquivo `.lock` e o mantém aberto.
Não há nenhuma chamada a `flock(2)`, `fcntl(F_SETLK)`, `LockFileEx` ou equivalente. No Windows o
`share_mode(FILE_SHARE_READ)` impede que **outro processo que use o mesmo modo** abra para escrita, o
que dá alguma proteção. **No Unix não há proteção alguma**: dois processos `slim` apontando para a mesma
sessão abrem o `.lock` simultaneamente, ambos leem o arquivo, ambos fazem reparo de torn-tail e ambos
acrescentam eventos → **corrupção do arquivo de sessão** (linhas intercaladas, sequências fora de ordem,
torn tails fabricados pelo reparo concorrente em `event_log.rs:63-77`).

Também: o arquivo `.lock` nunca é removido (lixo no diretório de sessões).

**Correção sugerida:** usar primitiva de lock real do SO. Em Unix:
`fs2::FileExt::try_lock_exclusive` ou `nix::fcntl::flock(fd, FlockArg::LockExclusiveNonblock)` no
handle retornado; em Windows: `LockFileEx` via `std::os::windows::io::AsRawHandle`. Em caso de falha de
aquisição, retornar um erro explícito ("sessão em uso por outro processo") em vez de seguir em frente.
O lock deve ser tomado **antes** de qualquer leitura/parse do arquivo de dados.

---

## 12. Task de compactação em background vazada em caminhos de erro

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\mod.rs`
- **Linhas:** spawn em 1473-1489; cancelamento correto em 1803-1837; caminhos que vazam: 1053, 1115, 1122, 1209, 1447, 1455, 1491, 1603, 2129

```rust
                    let task = tokio::spawn(async move {
                        run_background_compaction(
                            background_client,
                            plan,
                            cancellation,
                            task_progress,
                        )
                        .await
                    });
                    pending_background = Some(PendingBackgroundCompaction {
                        task,
                        progress,
                        request_bytes,
                        estimated_input_tokens,
                        tokens_before,
                        started: Instant::now(),
                    });
```
```rust
    async fn cancel_pending_background(
        &mut self,
        pending: &mut Option<PendingBackgroundCompaction>,
        next_seq: &mut u64,
        reason: &str,
    ) -> Result<UsageTotals, ProviderError> {
        let Some(mut attempt) = pending.take() else {
            return Ok(UsageTotals::default());
        };
```

- **Categoria:** bug
- **Severidade:** **alta** no agregado; cada caminho individual é **média**. Observação: `PendingBackgroundCompaction`
  **não** tem `impl Drop`, portanto nada aborta a task quando `pending_background` é descartado.

**Descrição:** o `JoinHandle` da compactação em background só é abortado/aguardado dentro de
`cancel_pending_background` (linha 1826). Todos os retornos antecipados por `?` **depois** do spawn
ignoram essa limpeza. Caminhos verificados no loop:
- `1115` e `1122` (`push_runtime_event(...)?` dentro de `if should_compact`);
- `1209` (`return Err(...)` "context window still exceeded after compaction");
- `1447` e `1455` (`push_runtime_event(...)?` ao iniciar a tentativa);
- `1491` (`push_runtime_event(...)?` no branch "skipped below break-even" — logo após o `take()` do plano,
  **antes** do spawn, então este é seguro; o risco está nos anteriores);
- `1603` (`push_runtime_event(...)?` em `RepeatedFailedTool`).

Nesses casos a task fica **destacada** (o `JoinHandle` é dropado sem abort), continua fazendo uma
requisição HTTP de streaming ao provider, mantém vivo o `Arc<Mutex<...>>` de progresso e o
`HttpProviderClient` clonado, e nunca é cancelada pelo `CancellationToken` (que só é observado dentro
da própria task, via `run_background_compaction`, e que continuará pendente se o run inteiro já
terminou). Além do desperdício de rede, o `CompactionHandle` fica preso em `Preparing`
(`handle.mark_preparing()` na linha 1445) sem nunca receber `background_failed()`, o que bloqueia
futuras preparações em背景 por `retry_after_turns`.

**Correção sugerida:** implementar `Drop for PendingBackgroundCompaction` que chame `self.task.abort()`,
e/ou encapsular o estado em uma estrutura com `cancel` explícito invocado por um guarda no topo do loop.
No mínimo, converter os `push_runtime_event(...)?` problemáticos em `if let Err(e) = ... { self.cancel_pending_background(...).await; return Err(e); }`.

---

## 13. `Lease::drop` fora de um runtime tokio → servidor nunca desligado

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\pool.rs`
- **Linhas:** 279-298

```rust
impl Drop for Lease {
    fn drop(&mut self) {
        if decrement_atomic_once(&self.leases).is_none() {
            return;
        }
        decrement_atomic(&self.pool.leases_total, 1);
        if self.leases.load(Ordering::Acquire) != 0 {
            return;
        }
        let pool = self.pool.clone();
        let key = self.key.clone();
        let leases = self.leases.clone();
        let expected = self.instance.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pool.release_if_zero(&key, &expected, &leases).await;
            });
        }
    }
}
```

- **Categoria:** bug
- **Severidade:** **média**

**Descrição:** o contador de leases é decrementado **antes** de se tentar obter o handle do runtime. Se
o `Lease` for descartado fora de um contexto tokio (ex.: em um `Drop` executado por uma thread de
`spawn_blocking`, durante o shutdown do runtime, ou em código síncrono de teste/setup), `try_current()`
falha e **nenhuma ação é tomada**: `release_if_zero` nunca roda, logo o timer de idle-shutdown
(`pool.rs:591-594`) nunca é armado.

Resultado: a entrada permanece em `state.entries` com `leases == 0` e `idle_task == None`. O processo
do language server fica vivo **para sempre** — não há mais nenhum caminho que o desligue, já que
`shutdown_if_idle` só é agendado por `release_if_zero`. É um leak permanente de processo + slot de
`max_servers`.

**Correção sugerida:** desacoplar a liberação do contexto de execução atual. Guardar um
`tokio::runtime::Handle` (fraco) na própria struct `Lease` no momento da aquisição (onde **há**
runtime garantido) e usá-lo no `Drop`, com fallback síncrono:

```rust
pub struct Lease {
    pool: Arc<LspProcessPool>,
    handle: tokio::runtime::Handle,   // capturado em acquire()
    ...
}
// em drop:
let _ = self.handle.spawn(async move { pool.release_if_zero(...).await; });
```
Como alternativa de segurança, fazer o idle-shutdown watchdog do pool varrer periodicamente entradas
com `leases == 0 && idle_task.is_none()`.

---

## 14. `Drop for ServerProcess` executa `taskkill`/`kill` síncrono; não há `Drop for LspProcessPool`

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\pool.rs` e `D:\Slim\crates\slim-lsp\src\process.rs`
- **Linhas:** `pool.rs:69-81`; `process.rs:88-99`

```rust
impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        if let Some(pid) = self.pid.filter(|pid| *pid != 0) {
            crate::process::kill_process_tree(pid);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}
```
```rust
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
}
```

- **Categoria:** bug
- **Severidade:** **média**

**Descrição:** dois problemas combinados.
1. `kill_process_tree` usa `std::process::Command::output()`, que **cria um processo e bloqueia até ele
   terminar**. Em `pool.rs:87` isso está corretamente embrulhado em `spawn_blocking`, mas no
   `Drop for ServerProcess` (`pool.rs:75`) é chamado **diretamente**. Se o `Drop` ocorrer numa thread
   do runtime tokio (o caso normal), a thread fica bloqueada por dezenas/centenas de milissegundos
   executando `taskkill.exe` — exatamente o tipo de bloqueio que o restante do arquivo evita.
2. Busca confirmada: **não existe `impl Drop for LspProcessPool`**. Se o `Arc<LspProcessPool>` for
   descartado sem que `close_all()` tenha sido chamado, o `HashMap` de entradas é destruído e cada
   `PoolEntry` dispara `ServerProcess::drop` → `taskkill` síncrono **N vezes em sequência**, uma por
   servidor vivo, sem `shutdown()` graceful e sem `wait_or_force_kill`. Os processos filhos são mortos
   à força e o pool perde a oportunidade de encerramento limpo. Em runtime sendo desmontado, o
   `spawn_blocking` interno também pode nunca executar.

**Correção sugerida:** (a) tornar `Drop for ServerProcess` não-bloqueante — apenas `start_kill()` no
child e, se possível, mover o kill da árvore para uma thread detached ou para o watchdog do pool;
(b) adicionar `impl Drop for LspProcessPool` que aborte todos os `idle_task` e, quando um handle de
runtime estiver disponível, chame `close_all()` (ou, no mínimo, que documente explicitamente que
`close_all()` é obrigatório antes do drop).

---

## 15. `max_servers` contabiliza entradas idle → `ServerLimit` falso

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\pool.rs`
- **Linhas:** 369-374

```rust
                    let occupied = state.entries.len().saturating_add(state.starting.len());
                    if occupied >= self.config.max_servers.max(1) {
                        return Err(PoolError::ServerLimit {
                            limit: self.config.max_servers.max(1),
                        });
                    }
```

- **Categoria:** bug
- **Severidade:** **média**

**Descrição:** `state.entries` inclui servidores com **zero leases** que ainda não atingiram o
`idle_shutdown` (padrão de 15 minutos, `pool.rs:205`). Com `max_servers: 4` (`pool.rs:206`), basta o
agente ter visitado 4 workspaces/raízes distintos na última janela de 15 min para que qualquer 5ª
aquisição retorne `ServerLimit`, **mesmo que todos os 4 estejam ociosos**. Não há promoção nem
evicção de LRU: o pool nunca desliga um servidor idle para abrir espaço para um novo.

O resultado prático é que `code_intel` começa a responder `language-server process limit reached (4)`
— erro opaco e aparentemente aleatório da perspectiva do usuário — em vez de reaproveitar um slot
ocioso.

**Correção sugerida:** contar apenas servidores efetivamente em uso (`entry.leases.load() > 0`) contra
`max_servers`; ou, quando o limite for atingido, escolher a entrada idle mais antiga, removê-la e
executar o shutdown em background (fora do lock, como já é feito em `shutdown_if_idle`) antes de
registrar o novo `starting`.

---

## 16. TOCTOU em `read_capped`: `metadata()` e `read_to_string()` são aberturas distintas

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\manager.rs`
- **Linhas:** 228-234 (`read_capped`), chamado em 242, 262, 288, 341, 1220, 1267

```rust
    fn read_capped(path: &Path) -> Option<String> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_CONTEXT_READ_BYTES as u64 {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }
```

- **Categoria:** segurança
- **Severidade:** **média**

**Descrição:** check-then-act clássico. `metadata()` e `read_to_string()` abrem o caminho **duas vezes**.
Entre as duas chamadas, o caminho pode ser substituído:
- por um **symlink** para `/etc/shadow`, `~/.ssh/id_rsa` etc. — `metadata()` segue symlinks, mas foi
  feito sobre o arquivo original; o `read_to_string` abre o alvo do symlink e **vaza seu conteúdo**;
- por um arquivo **grande** (ex.: `/dev/zero` ou um log de 50 GiB) — o limite de 4 MiB foi validado
  sobre o arquivo antigo, e `read_to_string` tenta carregar tudo em memória → **OOM**.

Isso é relevante porque `read_capped` é chamado **depois** de `crate::path_policy::existing_workspace_path`
(que canonicaliza e valida o confinamento ao workspace, em `manager.rs:241`), mas o caminho
canonicalizado é re-aberto por nome, reabrindo a janela. Observação: o exploit exige capacidade de
escrita no workspace, o que reduz o impacto — mas o workspace é justamente o diretório que o agente
edita livremente (incluindo aplicar patches sugeridos por terceiros).

Nota: `path_policy.rs` em si está correto (seeção "Verificação negativa"); o problema é o re-abrir
posterior.

**Correção sugerida:** abrir **uma única vez** e validar sobre o handle já aberto, usando
`OpenOptions::new().read(true).custom_flags(O_NOFOLLOW)` (Unix) / `FILE_FLAG_OPEN_REPARSE_POINT`
(Windows) para não seguir symlinks, depois `File::metadata()` (fstat, não segue links) e leitura com
`Read::take(MAX_CONTEXT_READ_BYTES + 1)` para impor o teto sobre o que é efetivamente lido:

```rust
fn read_capped(path: &Path) -> Option<String> {
    let mut f = open_nofollow(path).ok()?;          // single open
    let md = f.metadata().ok()?;                     // fstat
    if !md.is_file() || md.len() > MAX_CONTEXT_READ_BYTES as u64 { return None; }
    let mut buf = Vec::with_capacity(md.len() as usize);
    f.take(MAX_CONTEXT_READ_BYTES as u64 + 1).read_to_end(&mut buf).ok()?;
    if buf.len() > MAX_CONTEXT_READ_BYTES { return None; }
    String::from_utf8(buf).ok()
}
```
O padrão `open_checked` em `session/event_log.rs:236-249` já faz algo similar e pode ser reaproveitado.

---

## 17. Canal de progresso sem limite → memória ilimitada

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\mod.rs`
- **Linhas:** 2811, 2828-2877

```rust
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::task::spawn_blocking(move || {
            tools.execute_with_cancellation_and_progress(
                mode,
                &cwd,
                &name,
                &arguments,
                cancellation.as_ref(),
                |progress| {
                    let _ = progress_tx.send(progress);
                },
            )
        });
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** o callback de progresso é invocado pela ferramenta (executando em `spawn_blocking`, sem
backpressure) e empurra eventos num canal **ilimitado**. O consumidor drena esse canal dentro de
`tokio::select!` (linhas 2828-2858) e, para cada item, chama
`push_runtime_transient_event` → `AppHandle::push_transient_event`, que por sua vez pode **bloquear**
(ver achado #1) e também grava no `Vec` ilimitado (achado #10).

Se a ferramenta emitir progresso mais rápido do que o consumidor processa (ex.: um `shell` com saída
contínua, ou uma ferramenta que emite um preview por linha de um arquivo grande), a fila do canal
cresce sem limite até esgotar a memória. Não há `try_send`, não há limite de capacidade e não há
política de descarte/coalescência — note que `push_transient_event` **não** coalesce (usa `try_send`
com descarte silencioso em `Full`, `model.rs:411-414`), então o custo por item é integral.

**Correção sugerida:** trocar por `tokio::sync::mpsc::channel(CAPACITY)` (ex.: 64) e usar `try_send`
no callback, descartando o preview quando cheio (progresso é informativo e descartável por natureza).
Como alternativa, coalescer no próprio produtor mantendo apenas o preview mais recente
(`Arc<Mutex<Option<Preview>>>` + `Notify`), o que preserva a semântica "último estado" com custo O(1).

---

## 18. `store_dependency` descarta silenciosamente ao atingir 256 dependências

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\governor.rs`
- **Linhas:** 21, 520-544

```rust
const MAX_OBSERVED_DEPENDENCIES: usize = 256;
```
```rust
    fn track_dependency(&mut self, snapshot: &DependencySnapshot) -> bool {
        let changed = self
            .ledger
            .observed
            .get(&snapshot.key)
            .is_some_and(|known| known.digest != snapshot.digest);
        self.store_dependency(snapshot);
        changed
    }

    fn store_dependency(&mut self, snapshot: &DependencySnapshot) {
        if !self.ledger.observed.contains_key(&snapshot.key)
            && self.ledger.observed.len() >= MAX_OBSERVED_DEPENDENCIES
        {
            return;
        }
        self.ledger.observed.insert(
```

- **Categoria:** bug
- **Severidade:** **média**

**Descrição:** quando o mapa está cheio e a chave é nova, o snapshot é **silenciosamente descartado** —
sem métrica, sem log, sem observação. A partir daí:
- `track_dependency` devolve `changed = false` para todo arquivo novo, então o governor **nunca mais
  detecta mudança de dependência** → `workspace_revision` congela;
- como a chave nunca é inserida, o arquivo continuará sendo "não rastreado" para sempre (não há
  inserção parcial nem promoção posterior).

Isso corrompe silenciosamente a análise causal: a detecção de evidência repetida
(`ReusableEvidence`/`RepeatedFailure`, `governor.rs:471-475`) e a detecção de turno estagnado
(`finish_turn`, `governor.rs:307-343`) passam a operar sobre um conjunto de dependências truncado,
gerando tanto falsos positivos quanto falsos negativos de anomalia. O limite de 256 é facilmente
atingido em qualquer repositório de porte médio.

**Correção sugerida:** transformar `observed` em um LRU real (ex.: `LinkedHashMap` ou
`BTreeMap<String, ObservedDependency>` + `VecDeque` de ordem) e **evictar a entrada mais antiga** para
abrir espaço à nova, mantendo a capacidade. Alternativamente, registrar uma observação
`GovernorObservation::Anomaly { kind: DependencyTrackingDegraded }` quando o limite for atingido, para
que a degradação seja visível em vez de silenciosa.

---

## 19. `evidence` sem limite; `refresh_observed` não contabiliza bytes de diretório

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\governor.rs`
- **Linhas:** 78-79, 466-500 (`evidence`), 554-570 (`refresh_observed`)

```rust
struct ProgressLedger {
    workspace_revision: u64,
    uncertainty_epoch: u64,
    interaction_epoch: u64,
    internal_epoch: u64,
    observed: BTreeMap<String, ObservedDependency>,
    evidence: HashMap<String, EvidenceRecord>,
    evidence: HashMap<String, EvidenceRecord>,
```
```rust
        } else {
            self.ledger.evidence.insert(
                pending.call_fingerprint.clone(),
                EvidenceRecord {
                    evidence_id: evidence_id.clone(),
                    outcome,
                    repetitions: 0,
                },
            );
        }
```
```rust
        for (key, dependency) in dependencies {
            if matches!(dependency.kind, DependencyKind::File) {
                let bytes = fs::metadata(&dependency.path).map_or(0, |metadata| metadata.len());
                refreshed_bytes = refreshed_bytes.saturating_add(bytes);
                if refreshed_bytes > MAX_REFRESH_BYTES {
                    return Err(());
                }
            }
            let current = snapshot_path(dependency.kind, dependency.path.clone())?;
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** dois problemas no mesmo ledger:
1. `evidence: HashMap<String, EvidenceRecord>` só recebe inserções (`governor.rs:492`) e **nunca** é
   podada. A chave é `call_fingerprint` — que incorpora argumentos canônicos, digest de dependência,
   `workspace_revision` e `uncertainty_epoch` (`governor.rs:210-217`) — portanto é praticamente única
   por chamada. Numa sessão longa isso cresce sem limite (cada entrada guarda um `evidence_id: String`).
2. Em `refresh_observed`, o orçamento `refreshed_bytes` só acumula para `DependencyKind::File`. Para
   `DependencyKind::Directory`, `snapshot_path` → `fingerprint_directory` executa `fs::read_dir` com até
   `MAX_DIRECTORY_ENTRIES = 4096` entradas e um `metadata()` por entrada — **centenas de syscalls** —
   **sem contabilizar um único byte**. Com 256 diretórios observados, `refresh_observed` pode fazer
   mais de 1 milhão de syscalls síncronas antes de qualquer verificação de orçamento. O limite
   `MAX_REFRESH_BYTES = 32 MiB` simplesmente não se aplica a esse caso.

**Correção sugerida:** (1) limitar `evidence` (LRU com capacidade, ex.: 512) ou limpá-la em
`finish_turn` quando a entrada não for mais relevante; (2) estender o orçamento para cobrir
diretórios — contabilizar `entries_lidas` contra um `MAX_REFRESH_DIRECTORY_ENTRIES` global e retornar
`Err(())` (desclassificando a chamada, que é o comportamento seguro já previsto) quando estourar.

---

## 20. `refresh_observed` clona todo o `BTreeMap` a cada chamada

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\governor.rs`
- **Linhas:** 546-552

```rust
    fn refresh_observed(&mut self) -> Result<bool, ()> {
        let dependencies = self
            .ledger
            .observed
            .iter()
            .map(|(key, dependency)| (key.clone(), dependency.clone()))
            .collect::<Vec<_>>();
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** para evitar o borrow-checker, o código clona **todas** as chaves (String) e **todos** os
valores (`ObservedDependency` contém `PathBuf` + digest `String`) para um `Vec` temporário — até 256
pares, cada um com 2-3 alocações — **em cada chamada de `observe_before`**, ou seja, **em cada tool
call** de escopo `ObservedWorkspace`. Isso é desperdício puro: logo em seguida o loop já usa
`self.ledger.observed.get_mut(&key)` (linha 566), mostrando que o clone serviu apenas para contornar o
empréstimo.

**Correção sugerida:** iterar sem clonar, coletando apenas o que é estritamente necessário e aplicando
as mutações depois; ou reestruturar com `std::mem::take(&mut self.ledger.observed)` e devolvê-lo ao
final. Exemplo mínimo:

```rust
let keys: Vec<String> = self.ledger.observed.keys().cloned().collect();
for key in keys {
    let Some(dep) = self.ledger.observed.get(&key).cloned() else { continue };
    ...
}
```
(o clone do valor continua necessário por `snapshot_path` receber `PathBuf`, mas o da chave e o `Vec` de
pares somem). Melhor ainda: tornar `snapshot_path` `&Path`-based para eliminar o clone do `PathBuf`.

---

## 21. `parse` lê o arquivo inteiro e faz duplo parse; `restore_records` clona todos os records

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\jsonl_repo.rs` e `D:\Slim\crates\slim-core\src\session\capabilities.rs`
- **Linhas:** `jsonl_repo.rs:273-335`; `capabilities.rs:1658-1666`

```rust
pub(crate) fn parse(file: &mut File) -> io::Result<ParsedJsonl> {
    let file_len = file.metadata()?.len();
    if file_len > MAX_DURABLE_SESSION_BYTES {
        return Err(invalid_data(
            "durable session file exceeds the 64 MiB limit",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
```
```rust
        let value = match serde_json::from_slice::<Value>(line) {
            Ok(value) => value,
            Err(error) if error.is_eof() && !has_newline && is_final_line => {
                torn_tail = Some(bytes[offset..].to_vec());
                break;
            }
            Err(_) => return Err(invalid_data("invalid durable session JSON")),
        };
```
```rust
            let record =
                serde_json::from_value::<DurableRecord>(value).map_err(invalid_data_from)?;
```
```rust
    fn restore_records(&mut self) -> Result<(), CapabilityLedgerError> {
        let records = self.repo.records().to_vec();
        for record in records {
            let DurableRecord::Fact { fact, .. } = record else {
                continue;
            };
            let fact_bytes = serde_json::to_vec(&fact)
                .map_err(|_| CapabilityLedgerError::InvalidRecords("fact encoding".into()))?
                .len();
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** cadeia de ineficiências no caminho de abertura/retomada de sessão:
1. `parse` carrega o arquivo **inteiro** (até `MAX_DURABLE_SESSION_BYTES = 64 MiB`) num `Vec<u8>` antes
   de processar (linhas 280-282). Não há streaming/`BufReader`.
2. Cada linha é desserializada **duas vezes**: primeiro para `serde_json::Value` (linha 292) e depois
   `serde_json::from_value::<DurableRecord>(value)` (linha 316) — o que força a construção e o descarte
   de uma árvore `Value` completa por registro.
3. `restore_records` (`capabilities.rs:1659`) faz `self.repo.records().to_vec()` — clona **todos** os
   registros (cada um com payload completo) só para iterar — e, em seguida, para cada fact, chama
   `serde_json::to_vec(&fact)` (linha 1664) **apenas para medir o tamanho**, re-serializando algo que
   acabou de ser desserializado.

Tudo isso ocorre de forma síncrona, potencialmente dentro de contexto async (abrir/retomar sessão).

**Correção sugerida:** (1) usar `BufReader` + `read_line` para streaming, evitando materializar 64 MiB;
(2) desserializar diretamente para `DurableRecord` (`serde_json::from_slice::<DurableRecord>`), com
fallback para `Value` apenas quando o tipo precisar ser inspecionado antes (o campo `"type"` pode ser
lido com `serde_json::Deserializer::from_slice` + `RawValue`, ou com um struct-envelope leve
`#[derive(Deserialize)] struct Typed { r#type: String }`);
(3) iterar por referência em `restore_records` e medir o tamanho durante o parse original
(armazenando `fact_bytes` no registro) em vez de re-serializar.

---

## 22. Mapas de capacidade/children/idempotency crescem sem limite

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\capabilities.rs`
- **Linhas:** 610-621 (struct), 1369-1378 (`task_idempotency`), 868-876 (`capabilities`), 1243-1244 (`children`)

```rust
pub struct CapabilityService<R> {
    repo: R,
    catalog: CapabilityCatalog,
    capabilities: BTreeMap<String, CapabilityState>,
    capability_queue: VecDeque<String>,
    children: BTreeMap<String, ChildState>,
    child_queue: VecDeque<String>,
    tasks: BTreeMap<String, TaskState>,
    task_idempotency: BTreeMap<String, TaskMutationRequest>,
}
```
```rust
        self.task_idempotency
            .insert(request.idempotency_key.clone(), request.clone());
        self.tasks
            .entry(request.entity_id.clone())
            .or_insert_with(|| TaskState {
                revision: 0,
                mutations: Vec::new(),
            })
            .mutations
            .push(request.clone());
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** busca confirmada: **não há nenhum** `capabilities.remove(`, `children.remove(` ou
`task_idempotency.clear()`. Entradas são inseridas e nunca removidas, mesmo após atingir estado
terminal (`CapabilityExecutionState::Terminal`, linha 965; `DurableChildStatus` terminal; mutação
aplicada). Consequências:
- `capabilities` acumula uma entrada por capability já encerrada — cada uma com `CapabilityRequest`
  completo — para **toda a vida da sessão**;
- `children` idem;
- `task_idempotency` acumula uma entrada por mutação de todo, e é reconstruída integralmente a cada
  `restore_records` (`capabilities.rs:1861`);
- `tasks[..].mutations` é limitado a `MAX_TASK_COLLECTION = 128` (linha 1339), mas `task_idempotency`
  não tem limite correspondente.

Como o estado é reconstruído por `restore_records` a partir de **todos** os registros duráveis a cada
abertura de sessão, o consumo de memória cresce linearmente com o tamanho do arquivo JSONL (até 64 MiB)
e com o número de capabilities já despachadas.

**Correção sugerida:** (a) remover/podar entradas terminales (mantendo apenas uma janela recente para
fins de status) ou convertê-las em um resumo compacto; (b) limitar `task_idempotency` com o mesmo
`MAX_TASK_COLLECTION` e evictar as mais antigas (é um BTreeMap — a ordem de inserção não é preservada,
então use um `VecDeque` auxiliar de chaves para evicção FIFO); (c) considerar reconstrução lazy/preguiçosa
em `restore_records` em vez de materializar tudo.

---

## 23. Indexação `[]` sem verificação em mapa e Vec → pânico

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\attempts.rs`
- **Linhas:** 664-684

```rust
        match self.attempt_operations.get(attempt_id) {
            None => {
                return Err(AttemptLedgerError::AttemptNotStarted {
                    operation_id: operation_id.into(),
                    attempt_id: attempt_id.into(),
                });
            }
            Some(owner) if owner != operation_id => {
                return Err(AttemptLedgerError::AttemptOperationMismatch {
                    attempt_id: attempt_id.into(),
                    operation_id: operation_id.into(),
                });
            }
            Some(_) => {}
        }
        let position = self.attempt_positions[attempt_id];
        let attempt = &mut self
            .operations
            .get_mut(operation_id)
            .expect("owner was just checked")
            .attempts[position];
```

- **Categoria:** bug
- **Severidade:** **média**

**Descrição:** duas indexações que podem entrar em pânico, baseadas em invariantes implícitas:
1. `self.attempt_positions[attempt_id]` — o código verifica a existência da chave em
   `self.attempt_operations` (linha 664), mas indexa um **mapa diferente** (`attempt_positions`).
   São duas estruturas distintas (`attempts.rs:293-294`) que só coincidem por convenção. Se algum dia
   uma for inserida sem a outra — por um novo caminho de restore, um caminho de erro parcial, ou uma
   migração de schema — o `[]` entra em pânico.
2. `.attempts[position]` — `position` vem de `attempt_positions`, que guarda `state.attempts.len()`
   capturado no momento da inserção (`attempts.rs:564,574`). Se o `Vec` de attempts for algum dia
   truncado/reconstruído sem que `attempt_positions` seja recalculado, o índice estoura → pânico.
3. Há ainda `.expect("owner was just checked")` (linha 683) — um pânico redundante num caminho que já
   poderia retornar `Err`.

*Ressalva:* no código atual os três mapas/Vec são populados juntos no mesmo bloco (linhas 564-574), de
modo que o pânico **não é alcançável hoje**. Marca-se como média (e não alta) justamente por isso: é
uma invariante frágil que vira pânico no primeiro desalinhamento, num módulo cujo contrato é
"falhar fechado" com erros tipados.

**Correção sugerida:** substituir as indexações por acessos que propagam erro:

```rust
let position = self.attempt_positions.get(attempt_id).copied().ok_or_else(|| {
    AttemptLedgerError::AttemptNotStarted { operation_id: operation_id.into(), attempt_id: attempt_id.into() }
})?;
let state = self.operations.get_mut(operation_id).ok_or_else(|| {
    AttemptLedgerError::AttemptNotStarted { operation_id: operation_id.into(), attempt_id: attempt_id.into() }
})?;
let attempt = state.attempts.get_mut(position).ok_or_else(|| {
    AttemptLedgerError::AttemptNotStarted { operation_id: operation_id.into(), attempt_id: attempt_id.into() }
})?;
```
Melhor ainda: fundir `attempt_operations` e `attempt_positions` num único mapa
`BTreeMap<String, (String, usize)>` para tornar a invariante impossível de violar por construção.

---

## 24. `mem::replace` do `AppHandle` sem guarda de pânico

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\mod.rs`
- **Linhas:** 2934, 2961

```rust
        let sensitive_values = self.sensitive_values.0.clone();
        let mut app = std::mem::replace(&mut self.app, AppHandle::fake());
        let mut progress_error = None;
        let mut result = self.tools.execute_with_cancellation_and_progress(
            mode,
            cwd,
            invocation.name,
            invocation.arguments,
            self.cancellation.as_ref(),
            |progress| { ... },
        );
        self.app = app;
```

- **Categoria:** bug
- **Severidade:** **média** (impacto real reduzido — ver ressalva)

**Descrição:** o `AppHandle` real é retirado de `self.app` e substituído por um *fake* para permitir que
o callback de progresso faça borrow mutável sem conflitar com `&mut self`. O problema é que
`self.app = app` (linha 2961) só executa no caminho normal. Se
`tools.execute_with_cancellation_and_progress(...)` **entrar em pânico**, ou se o closure capturado
entrar em pânico, o `AppHandle` real — que contém **todos os eventos da sessão** (achado #10) —
é dropado junto com `app`, e `self.app` permanece como `AppHandle::fake()`.
A partir daí todos os eventos subsequentes são descartados silenciosamente: a sessão parece funcionar
mas não registra nada, e o `SessionWriter` nunca recebe os eventos.

*Ressalva:* `execute_tool_call` só é alcançado por `Runtime::execute_tool` (linha 2759), cujos únicos
consumidores no repositório são testes (`slim-core/tests/runtime_abort.rs:164`,
`slim-core/tests/tool_lifecycle_identity.rs:10`). Portanto o impacto em produção é hoje nulo; a
severidade reflete o risco caso esse caminho passe a ser usado, e o fato de o padrão poder ser copiado.

**Correção sugerida:** nunca deixar `self.app` em estado inválido entre pontos de pânico. Usar um
guard de escopo:

```rust
struct AppRestore<'a> { slot: &'a mut AppHandle, value: Option<AppHandle> }
impl Drop for AppRestore<'_> {
    fn drop(&mut self) {
        if let Some(v) = self.value.take() { *self.slot = v; }
    }
}
```
e, preferencialmente, reestruturar para que o callback receba `&mut AppHandle` por parâmetro
(evitando o `mem::replace`), como já é feito em `execute_tool_call_async` (linha 2843) — que é a
versão async e correta do mesmo padrão.

---

## 25. `redact_values` faz N cópias completas do input

- **Arquivo:** `D:\Slim\crates\slim-core\src\runtime\mod.rs`
- **Linhas:** 3848-3854 (chamado em 711, 2103, 2265, 2472, 2864, 3016-3037, 4347, 4393)

```rust
fn redact_values(sensitive_values: &[String], input: &str) -> String {
    sensitive_values
        .iter()
        .fold(input.to_owned(), |redacted, value| {
            redacted.replace(value, "[REDACTED]")
        })
}
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** `String::replace` aloca uma **nova String completa** a cada iteração. Com N valores
sensíveis registrados, são N alocações do tamanho do input e N varreduras completas — O(N × |input|).
Onde isso dói:
- `redact_messages` (linhas 3011-3042) clona **todas** as mensagens e redacta cada campo de cada uma,
  **a cada início de loop de agente** (chamado em `run_agent_loop_with_messages:965` e em
  `compact_before_send`);
- `redact_sensitive(&outcome.result.output)` (linha 2103 etc.) sobre outputs de ferramenta que podem
  ter centenas de KiB;
- `take_redacted_stream_chunk` (linha 4347) sobre **cada delta de streaming**.

Somando-se ao achado #10 (os outputs também são clonados no `Vec` de eventos), uma mensagem grande
pode ser copiada dezenas de vezes por turno.

**Correção sugerida:** fazer uma única passagem, construindo a saída uma vez. Como os valores já são
mantidos ordenados por tamanho decrescente (`register_sensitive_value`, linhas 700-708), é possível
usar Aho-Corasick (crate `aho-corasick`) para substituir todos os padrões em uma passagem, ou,
no mínimo, evitar a cópia quando nada casa:

```rust
fn redact_values(sensitive_values: &[String], input: &str) -> String {
    if sensitive_values.is_empty() { return input.to_owned(); }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    'outer: while !rest.is_empty() {
        for value in sensitive_values {
            if let Some(pos) = rest.find(value.as_str()) {
                out.push_str(&rest[..pos]);
                out.push_str("[REDACTED]");
                rest = &rest[pos + value.len()..];
                continue 'outer;
            }
        }
        out.push_str(rest);
        break;
    }
    out
}
```
Isso reduz para O(|input| × N) no pior caso com **uma única alocação**, e para O(|input|) quando
houver poucos valores (a busca linear pelos padrões é curta-circuited pelo `find`).

---

## 26. TOCTOU em `resolve_session_path` e `open_checked`

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\event_log.rs`
- **Linhas:** 215-234, 236-249

```rust
pub(crate) fn resolve_session_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    if path.exists() {
        fs::canonicalize(path)
    } else {
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if is_reparse_metadata(&metadata) {
                return Err(reparse_error(path));
            }
        }
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session path has no file name")
        })?;
        Ok(fs::canonicalize(parent)?.join(file_name))
    }
}
```
```rust
pub(crate) fn open_checked(mut options: OpenOptions, path: &Path) -> io::Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if is_reparse_metadata(&metadata) {
            return Err(reparse_error(path));
        }
    }
    #[cfg(windows)]
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    if is_reparse_metadata(&file.metadata()?) {
        return Err(reparse_error(path));
    }
    Ok(file)
}
```

- **Categoria:** segurança
- **Severidade:** **média**

**Descrição:** ambos são check-then-act sobre o filesystem, com a checagem e o uso em syscalls distintas:
1. `resolve_session_path`: `path.exists()` → `fs::canonicalize(path)` (abertura separada). No ramo
   `else`, a checagem `fs::symlink_metadata(path)` é feita sobre `path`, mas o resultado é montado com
   `fs::canonicalize(parent)?.join(file_name)` — o `parent` é canonicalizado **depois** de o
   `symlink_metadata` ter sido avaliado, então um atacante pode trocar o diretório por um symlink entre
   as duas chamadas.
2. `open_checked`: `fs::symlink_metadata(path)` → `options.open(path)`. A janela entre as duas permite
   substituir o caminho por um symlink. A re-checagem final com `file.metadata()` **não** resolve o
   problema em Unix: `File::metadata()` usa `fstat` sobre o arquivo **já aberto**, e após `open()` ter
   seguido o symlink o `fstat` reporta o alvo (um arquivo regular, não um symlink) → a checagem passa.
   No Windows a mitigação é melhor porque `FILE_FLAG_OPEN_REPARSE_POINT` impede seguir o link, mas a
   primeira checagem anterior ao `open` ainda é racy.

O impacto é limitado pela ausência de lock real (#11) e porque o exploit exige controle sobre o
diretório de sessões. Ainda assim, este é o mecanismo de defesa documentado do módulo
("reparse-point session path is not allowed") e ele é contornável por corrida.

**Correção sugerida:** eliminar a checagem prévia e fazer tudo sobre um **único handle aberto**:
- abrir com `O_NOFOLLOW` (Unix) / `FILE_FLAG_OPEN_REPARSE_POINT` (Windows) para que `open` **falhe** em
  vez de seguir o link;
- depois, usar `fstat` no handle (`File::metadata()`) para validar `is_file()` e o reparse point.
Em `resolve_session_path`, canonicalizar o `parent` **uma vez** e derivar o caminho final desse
resultado, validando em seguida com `symlink_metadata` sobre o caminho final (aceitando a janela
residual ou reaplicando `O_NOFOLLOW` no `open` seguinte).

---

## 27. `claim_next` descarta `operation_id` silenciosamente → dessincronia → pânico depois

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\queue.rs`
- **Linhas:** 189-195, 376-386

```rust
    pub fn claim_next(&mut self) -> Option<QueueItem> {
        let operation_id = self.pending.pop_front()?;
        let entry = self.entries.get_mut(&operation_id)?;
        debug_assert_eq!(entry.status, QueueStatus::Queued);
        entry.status = QueueStatus::Claimed;
        Some(entry.item.clone())
    }
```
```rust
        let item = self
            .entries
            .get(&operation_id)
            .map(|entry| entry.item.clone())
            .expect("pending queue entry");
        self.append_operation(repo, &operation_id, DurableOperationKind::Claimed)?;
        self.pending.pop_front();
        self.entries
            .get_mut(&operation_id)
            .expect("pending queue entry")
            .status = QueueStatus::Claimed;
```

- **Categoria:** bug
- **Severidade:** **média**

**Descrição:** em `claim_next`, o `pop_front()` (linha 190) **já removeu** o `operation_id` de `pending`
quando o `self.entries.get_mut(&operation_id)?` (linha 191) é avaliado. Se a entrada não existir, a
função retorna `None` — mas o `operation_id` **já foi perdido** de `pending`. O item fica inalcançável:
não está mais na fila de pendentes e não pode ser reivindicado por `claim(id)` (que exige o id
explicitamente).

O dano colateral é pior: `claim_next_persisted` (linhas 376-386) faz `self.pending.front()` e depois
`self.entries.get(&operation_id).map(...).expect("pending queue entry")` — ou seja, a mesma suposição de
sincronia entre `pending` e `entries`, mas aqui transformada em **pânico**. Qualquer dessincronia
introduzida por `claim_next` (ou por um caminho de restore parcial) faz `claim_next_persisted` entrar
em pânico em vez de retornar erro.

Há também `debug_assert_eq!` (linha 192): em builds de release essa invariante **não é verificada**, de
modo que um item em estado indevido pode ser reivindicado silenciosamente.

**Correção sugerida:** tornar a operação atômica — só remover de `pending` **depois** de confirmar a
entrada — e transformar o `expect` em erro propagado:

```rust
pub fn claim_next(&mut self) -> Option<QueueItem> {
    let operation_id = self.pending.front()?.clone();
    if !self.entries.contains_key(&operation_id) {
        self.pending.pop_front();          // descarta conscientemente + métrica
        return None;
    }
    self.pending.pop_front();
    let entry = self.entries.get_mut(&operation_id)?;
    ...
}
```
Em `claim_next_persisted`, usar `ok_or(DurableQueueError::UnknownOperation { .. })?` no lugar dos dois
`expect`. Adicionalmente, documentar e testar a invariante `pending ⊆ entries.keys()`.

---

## 28. Leitura do header LSP byte a byte (1 syscall por byte)

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\transport.rs`
- **Linhas:** 393-410

```rust
    let mut header = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        let read = reader
            .read(&mut byte)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))?;
        if read == 0 {
            return Ok(None);
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 4096 {
            return Err(TransportError::Protocol("header exceeds 4096 bytes".into()));
        }
    }
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** cada byte do header custa **um `.await` completo sobre o reader**. Mesmo com
`tokio::io::split`, cada `read(&mut [u8;1])` é uma chamada de sistema (ou, no melhor caso, uma
verificação de buffer do `BufReader` — mas aqui não há `BufReader`). Um header típico de LSP tem
~40 bytes, então são ~40 awaits + 40 syscalls por mensagem. Como `read_loop` (`transport.rs:488-566`)
processa **todas** as mensagens do servidor em série, isso é o gargalo de toda a camada LSP.

Agrava: a cada iteração há também uma comparação `ends_with(b"\r\n\r\n")` (O(4), desprezível) e o loop
de parsing do header (linhas 413-424) faz `split("\r\n")` com alocação.

**Correção sugerida:** envolver o reader num `tokio::io::BufReader` e ler linha a linha com
`read_until(b'\n')`, ou ler em blocos de 128 bytes e usar `memchr::memmem` para localizar `\r\n\r\n`.
Exemplo mínimo com o que já existe no crate:

```rust
let mut header = Vec::with_capacity(128);
let mut buf = [0u8; 64];
loop {
    let n = reader.read(&mut buf).await?;
    if n == 0 { return Ok(None); }
    header.extend_from_slice(&buf[..n]);
    if let Some(end) = header.windows(4).position(|w| w == b"\r\n\r\n") { ... break; }
    if header.len() > 4096 { return Err(...); }
}
```
Isso reduz de ~40 para 1 syscall por header.

---

## 29. `document_sync` mantido durante a escrita no servidor

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\instance.rs`
- **Linhas:** 342, 399, 479 (e o `_document_sync` mantido até o fim de cada função)

```rust
    pub async fn sync_document(&self, path: &Path, text: String) -> Option<i64> {
        let _document_sync = self.document_sync.lock().await;
        let path = crate::path_policy::existing_workspace_path(&self.config.root, path)?;
        let language_id = self.config.spec.language_id_for(&path)?;
        let uri = file_uri(&path)?;
        let update = {
            let mut state = self.state.lock().await;
            state.documents.upsert(path, uri.clone(), language_id, text)
        };

        match update {
            DocumentUpdate::Opened { document, evicted } => {
                Self::send_evicted_did_closes(&self.transport, &evicted).await;
```

- **Categoria:** performance
- **Severidade:** **média**

**Descrição:** `document_sync` é um `tokio::sync::Mutex<()>` que serializa a sequência
didOpen/didChange/didSave/didClose — o que é correto e intencional. O problema é o **escopo**: o guard
é mantido durante `Self::send_evicted_did_closes(...).await` (linha 353) e durante
`self.transport.notify(...).await` (linhas 362-368), que **escrevem no stdin do servidor**.
`transport.notify()` (linha 685) por sua vez adquire o mutex do writer e faz `write_all` + `flush`.

Se o pipe de stdin do language server estiver cheio (servidor ocupado indexando e momentaneamente sem
ler), o `write_all` pode bloquear **enquanto se mantém `document_sync`**, serializando todas as
sincronizações de documento daquela instância e, em cascata, atrasando as consultas que esperam por
elas. Com `max_open_documents` e múltiplos `didOpen` em voo numa inicialização, isso vira fila.

Observação positiva: a ordem de aquisição é sempre `document_sync → state → writer`, sem ciclo, então
**não há deadlock** — é um problema de latência/escorregamento de fila, não de travamento.

**Correção sugerida:** separar a seção crítica do I/O. Fazer o upsert e o cálculo de `evicted` sob
`state` dentro de um bloco curto (já é o caso), soltar `state`, e manter `document_sync` — mas
**limitar o tempo** da escrita com `tokio::time::timeout`, liberando `document_sync` (e marcando a
instância como degradada) se o servidor não absorver a escrita em, digamos, 5 s. Alternativamente,
manter `document_sync` apenas ao redor do upsert e enviar as notificações numa fila ordenada externa
(um único task de escrita por instância), o que preserva a ordem do protocolo sem manter o lock.

---

## 30. Evicção arbitrária de URI no `DiagnosticsStore`

- **Arquivo:** `D:\Slim\crates\slim-lsp\src\diagnostics.rs`
- **Linhas:** 45-49

```rust
        if !self.by_uri.contains_key(&uri) && self.by_uri.len() >= self.max_uris {
            if let Some(oldest) = self.by_uri.keys().next().cloned() {
                self.by_uri.remove(&oldest);
            }
        }
```

- **Categoria:** bug
- **Severidade:** **baixa**

**Descrição:** o comentário da função (`diagnostics.rs:38`) e o doc do módulo (linhas 3-4) sugerem
uma política de limites bem definida, mas a evicção usa `self.by_uri.keys().next()` sobre um
`HashMap` — cuja ordem de iteração é **arbitrária e não determinística**. Não é LRU nem "oldest";
é efetivamente aleatória. Na prática, ao atingir `max_uris = 256`, um URI que acabou de ser consultado
pode ser o escolhido para remoção, e o `code_intel diagnostics` subsequente para aquele arquivo
retorna vazio (falso negativo).

Impacto real pequeno (diagnósticos são recalculados pelo servidor e republicados), mas o
comportamento é não determinístico, o que torna bugs difíceis de reproduzir.

**Correção sugerida:** usar uma estrutura ordenada por inserção (ex.: manter `VecDeque<Url>` em
paralelo, ou trocar `HashMap` por `LinkedHashMap`/`IndexMap`) e remover efetivamente o **mais antigo**,
atualizando a ordem em `get`/`set`. Se a evicção aleatória for aceitável por design, corrigir o
comentário e o doc do módulo para não prometer "oldest".

---

## 31. `.expect()` em caminhos de produção no `CapabilityService`

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\capabilities.rs`
- **Linhas:** 938, 964, 966, 1013, 1033, 1081, 1126, 1148

```rust
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked before claim")
            .state = CapabilityExecutionState::Claimed;
```
```rust
        self.capabilities
            .get_mut(queue_id)
            .expect("capability state checked before append")
            .state = CapabilityExecutionState::Terminal(status);
        let state = self.capabilities.get(queue_id).expect("state exists");
```
```rust
        self.tasks
            .get_mut(&request.entity_id)
            .expect("task state inserted")
            .revision = request.revision;
```

- **Categoria:** estilo
- **Severidade:** **baixa**

**Descrição:** oito `expect()` com mensagens do tipo "checked before" em métodos públicos de produção.
Todos são precedidos por uma consulta que torna o pânico **hoje inalcançável** — por isso a severidade
é baixa. O problema é duplo: (a) são panic points num módulo cujo contrato é retornar
`CapabilityLedgerError` tipado para **todas** as falhas (inclusive corrupção de estado: já existem
`InvalidRecords`, `UnknownQueueId`, `InvalidCapabilityTransition`); (b) a invariante é mantida apenas
pela distância textual entre o `get` e o `get_mut` — qualquer refatoração que interponha uma mutação
(p.ex. um `append_fact` que falhe e limpe o mapa) transforma o pânico em alcançável.

A linha 966 (`let state = self.capabilities.get(queue_id).expect("state exists");`) é redundante:
o valor já foi obtido duas linhas acima.

**Correção sugerida:** converter em erros tipados, que é o estilo do resto do arquivo:

```rust
let state = self.capabilities.get_mut(queue_id).ok_or_else(|| {
    CapabilityLedgerError::UnknownQueueId(queue_id.to_owned())
})?;
state.state = CapabilityExecutionState::Claimed;
```
e eliminar a consulta redundante da linha 966 reusando o guard ou clonando os dois campos necessários.

---

## 32. `append_fact` serializa o fact duas vezes (uma só para medir o tamanho)

- **Arquivo:** `D:\Slim\crates\slim-core\src\session\capabilities.rs`
- **Linhas:** 1642-1656

```rust
        let fact = DurableFact {
            namespace: namespace.to_owned(),
            key,
            value,
        };
        let fact_bytes = serde_json::to_vec(&fact)
            .map_err(|_| CapabilityLedgerError::InvalidIdentifier("fact"))?
            .len();
        if fact_bytes > MAX_FACT_BYTES {
            return Err(CapabilityLedgerError::InvalidIdentifier("fact"));
        }
        self.repo
            .append(DurableRecord::Fact { seq, fact })
            .map_err(|error| CapabilityLedgerError::Persist(error.to_string()))
```

- **Categoria:** performance
- **Severidade:** **baixa**

**Descrição:** o fact é serializado integralmente para `Vec<u8>` apenas para verificar
`fact_bytes > MAX_FACT_BYTES` (16 KiB); em seguida `repo.append(...)` serializa o mesmo fact **de novo**
(dentro de `JsonlRepo::append_batch` → `encode_line`, `jsonl_repo.rs:230`). São duas serializações
completas por fact, e cada dispatch de capability grava 2-3 facts (`intent`, `claim`, `terminal`),
então são 4-6 serializações por operação de capability.

O mesmo padrão se repete em `restore_records` (linhas 1664-1666), onde cada fact é re-serializado
apenas para validar o tamanho durante o replay.

**Correção sugerida:** serializar uma vez e reusar:

```rust
let bytes = serde_json::to_vec(&fact).map_err(|_| CapabilityLedgerError::InvalidIdentifier("fact"))?;
if bytes.len() > MAX_FACT_BYTES { return Err(CapabilityLedgerError::InvalidIdentifier("fact")); }
self.repo.append_bytes(DurableRecord::Fact { seq, fact }, bytes)   // novo método que reusa o buffer
```
ou, se alterar a trait `DurableRepo` for demais, estimar o tamanho sem serializar (não é exato, mas para
um limite de segurança basta) ou aceitar o custo e mover a checagem para dentro de
`JsonlRepo::append_batch`, que já precisa serializar — eliminando a duplicação.

---

## Apêndice A — Padrões corretos encontrados (para referência / não corrigir)

Estes pontos foram verificados e estão **corretos**; registrados aqui para evitar "correções" que os
quebrem:

1. **`slim-lsp/src/transport.rs:102-107`** — `NotificationMailboxInner::lock()` usa
   `unwrap_or_else(|poisoned| poisoned.into_inner())`, e **todos** os guards do `StdMutex` são
   explicitamente dropados antes de qualquer `.await` (linhas 145-147, 156-159, 192-193, 269-270,
   291-292, 306-308). Nenhum `std::sync::Mutex` é mantido através de await.
2. **`slim-lsp/src/transport.rs:281-292`** — `NotificationReceiver::recv` cria o `notified`, chama
   `.enable()` **antes** de inspecionar a fila, e só então faz `.await`. Sem lost wakeup. (Contrastar
   com o achado #2.)
3. **`slim-core/src/context/compact.rs:274-287`** — `with_state`/`with_state_mut` tratam envenenamento
   com `poisoned.into_inner()`. (Contrastar com o achado #3.)
4. **`slim-core/src/runtime/mod.rs:3627-3630` e `3652-3655`** — tratamento correto de poisoning e
   nenhum await sob o guard, apesar de usarem `std::sync::Mutex`.
5. **`slim-lsp/src/pool.rs:345-380`** — singleflight de inicialização com `Arc<Notify>` e
   `Arc::ptr_eq` para detectar liderança roubada; o estado do pool nunca é mantido durante a
   inicialização do processo (linha 380 → `start_server` fora do lock).
6. **`slim-lsp/src/instance.rs:611-627`** — `shutdown()` com grace period via `CancellationToken` +
   `select!`, e `Drop` que aborta `read_task`/`drain_task` (`transport.rs:731-734`,
   `instance.rs:626`). Sem leak de task.
7. **`slim-lsp/src/path_policy.rs`** — confinamento ao workspace correto (canonicalização de raiz e
   candidato + `Path::starts_with` por componente). Ver seção "Verificação negativa".
8. **`slim-core/src/session/jsonl_repo.rs:216-262`** — `append_batch` valida o orçamento de tamanho de
   **todo** o lote antes de escrever (linhas 238-250) e marca `poisoned` em caso de falha, impedindo
   escritas parciais subsequentes. Boa disciplina.

---

## Apêndice B — Metodologia e cobertura

- Todos os arquivos do escopo foram lidos integralmente, com exceção de blocos `#[cfg(test)]` e de
  regiões puramente declarativas de `runtime/mod.rs` (structs/`enum`s de dados, linhas 100-560 parcial e
  4100-4330 parcial), que foram inspecionadas por `grep`.
- Trechos citados foram copiados literalmente dos resultados de `Read`/`Grep`; nenhum número de linha
  foi estimado.
- Buscas realizadas para fundamentar as conclusões:
  - `.lock()/.read()/.write() + .unwrap()/.expect()` em `crates/` — 6 ocorrências, todas em
    `slim-cli/src/tui.rs` (fora do escopo) e `slim-core/src/model.rs` (achado #3).
  - `unwrap_or_else(|e| e.into_inner())` em `crates/` — presente em `transport.rs`, `runtime/mod.rs`
    e `context/compact.rs`; **ausente** em `model.rs` (achado #3).
  - `std::sync::Mutex`/`RwLock` em `crates/` — apenas `slim-cli/*`, `slim-core/src/model.rs` e
    `slim-lsp/src/transport.rs`.
  - `unreachable!`/`todo!`/`unimplemented!` nos arquivos do escopo — **zero**.
  - `impl Drop for` em `slim-lsp/src/` — 5 ocorrências; **nenhuma** para `LspProcessPool` (achado #14).
  - `drain_events`/`SessionWriter::create` — `drain_events` só em `slim-cli/src/headless.rs:1124`
    (achado #10).

### Lacunas conhecidas desta auditoria

- Não foi executado `cargo build`/`cargo clippy`; todas as conclusões são de análise estática de leitura.
- `crates/slim-core/src/model.rs` e `crates/slim-core/src/context/compact.rs` estão **fora** do escopo
  nominal, mas foram incluídos quando diretamente implicados em achados do escopo
  (`AppHandle`/`SessionEventSender` em #1, #3, #10; `CompactionHandle`/`take_prepared` como verificação
  negativa).
- Não foram analisados: `crates/slim-core/src/session/tool_phases.rs` (781 linhas),
  `session/observability_*.rs` (1.2k linhas), `session/manual_drive.rs`, `session/resume.rs`,
  `slim-lsp/src/position.rs` e `slim-lsp/src/document.rs` em profundidade — apenas varredura por padrões
  de pânico/concorrência.
