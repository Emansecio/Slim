# Relatório de Auditoria Estática — Slim Agent

**Data:** 2026-08-30
**Escopo:** análise estática (somente leitura) do código Rust do workspace `D:\Slim`
**Base analisada:** 205 arquivos `.rs`, ~92.000 linhas, 4 crates (`slim-core`, `slim-cli`, `slim-tui`, `slim-lsp`), edition 2021
**Nenhum arquivo do projeto foi modificado.**

---

## Metodologia e honestidade dos dados

Foram usadas três técnicas complementares:

1. **Varredura por padrões** (grep) sobre `unsafe`, `Command::new`, `env::set_var`, `unwrap`/`expect`,
   travamento (`lock`), alocações em laço e limites de buffer.
2. **Leitura direta** dos módulos de maior risco: `auth.rs` (98 blocos `unsafe` de manipulação de ACL
   do Windows), `oauth/pkce.rs`, `oauth/store.rs`, `process.rs`, `mcp/stdio.rs`, `tools/mod.rs`,
   `model.rs`, `governor.rs`, `session/queue.rs`, `tools/read.rs`.
3. **Varredura automatizada assistida** sobre `tools/`, `context/`, `provider*`, `mcp/` e `lsp/`.

**Distinção importante:** cada achado das seções "Alta", "Média" e "Baixa" foi confirmado por leitura
direta do trecho citado. Os achados que **não** foram confirmados por leitura estão isolados na
seção [Achados não verificados](#achados-não-verificados-varredura-automatizada) e **não entram nas
contagens do resumo executivo**.

**Falsos positivos descartados durante a auditoria** (documentados para evitar retrabalho):

| Alegação inicial | Veredito |
|---|---|
| `env::set_var` sem `unsafe` é UB (headless.rs, tui.rs) | **Falso positivo.** Todas as ocorrências estão dentro de `#[cfg(test)]` e protegidas por um `ENV_LOCK` global. Nenhuma mutação de ambiente em código de produção. |
| `tools/read.rs:215` `canonicalize().expect()` em produção | **Falso positivo.** Está dentro de `#[cfg(test)] fn checkpoint_count`. |
| Snapshots de busca retêm ~640 MB | **Incorreto.** `SearchHit` guarda apenas `PathBuf + usize + String` (uma linha). Pior caso real: ~12 MB (10.000 hits × 8 snapshots). Limites `MAX_SEARCH_SNAPSHOT_HITS`/`MAX_SEARCH_SNAPSHOTS` estão corretos. |
| Contagem de tokens é O(n²) | **Exagerado.** `estimate_provider_message_tokens` é O(n) por chamada; o custo real é O(n·k) por k varreduras repetidas no mesmo turno. |
| `session/queue.rs` — 10 `expect()` | **Confirmado em produção**, mas protegidos por verificação prévia (`self.entry(...)?`). Rebaixado para "estilo". |

---

## Resumo executivo

| Severidade | Quantidade |
|---|---|
| **Crítica** | 0 |
| **Alta** | 2 |
| **Média** | 11 |
| **Baixa** | 9 |
| **Total verificado** | **22** |

### Totais por categoria (achados verificados)

| Categoria | Crítica | Alta | Média | Baixa | Total |
|---|---|---|---|---|---|
| Segurança | 0 | 2 | 5 | 1 | **8** |
| Bug | 0 | 0 | 4 | 3 | **7** |
| Performance | 0 | 0 | 2 | 4 | **6** |
| Estilo | 0 | 0 | 0 | 1 | **1** |
| **Total** | **0** | **2** | **11** | **9** | **22** |

### Panorama

O codebase é **substancialmente melhor do que a média** para projetos deste porte: não há `unsafe`
desnecessário fora da camada de ACL do Windows (que é cuidadosa e bem verificada), o PKCE é
criptograficamente correto, a resolução de executáveis usa caminhos absolutos, e o tratamento de
envenenamento de lock segue um padrão consistente (`unwrap_or_else(|p| p.into_inner())`).

Os problemas reais concentram-se em três temas:

1. **Contenção de workspace inconsistente** — `code_intel` valida, as ferramentas centrais de
   arquivo não. O próprio prompt de sistema do projeto declara que arquivos são "evidência não
   confiável", mas a camada de ferramentas não aplica essa regra.
2. **Redação de segredos incompleta** — cobre 2 cabeçalhos e falha em corpos JSON.
3. **Pânico como resposta a falhas transitórias** — 122 `unwrap()`/`expect()` em código de produção,
   vários em laços de eventos centrais, com envenenamento de lock não recuperável.

**Não foi encontrada nenhuma vulnerabilidade crítica.** Não há injeção de shell por interpolação de
string, não há `unsafe` arbitrário em código de parsing, e os limites de rede e de tamanho de
resposta estão consistentemente aplicados.

---

# ACHADOS VERIFICADOS

## Severidade ALTA

### A-01 — Ferramentas de arquivo não confinam paths ao workspace

- **Arquivo:** `crates/slim-core/src/tools/mod.rs`
- **Linhas:** 795-802 (definição); usado em 486, 510, 557, 578, 597
- **Categoria:** Segurança
- **Severidade:** Alta

```rust
fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
```

**Descrição.** Não há canonicalização, nem resolução de `..`, nem verificação de contenção
(`starts_with`). Um path absoluto passa direto; um relativo com `..` escapa do `cwd`. Isso afeta as
ferramentas centrais de leitura, listagem, escrita e patch (`mod.rs:486`, `510`, `557`, `578`, `597`).

O agravante é que **o mesmo repositório já resolve esse problema corretamente** em
`crates/slim-core/src/tools/code_intel.rs:92`, cuja documentação diz explicitamente: *"Reject
absolute paths from the model — all paths must be relative to the workspace root so we can enforce
containment"*. Há, portanto, duas implementações do mesmo conceito com posturas de segurança
opostas.

Impacto concreto: um modelo comprometido por prompt injection (o vetor que o próprio
`NATIVE_SYSTEM_PROMPT` do projeto reconhece ao dizer que arquivos e saídas de ferramentas são
"evidência não confiável") pode ler qualquer arquivo do usuário — incluindo
`%USERPROFILE%\.slim\auth.json`, que contém as chaves de API e tokens OAuth em texto puro.

**Correção sugerida.** Extrair `safe_resolve_path` de `code_intel.rs` para um módulo compartilhado e
usá-lo em todos os cinco call sites, rejeitando paths absolutos e exigindo contenção por
`canonicalize()` com fallback lexical. O fallback lexical de `code_intel.rs` também merece atenção
pois não resolve symlinks (ver B-04).

---

### A-02 — Redação de segredos incompleta e ineficaz sobre JSON

- **Arquivo:** `crates/slim-cli/src/auth.rs`
- **Linhas:** 518-583 (`redact_header_line`, `find_sensitive_header`), 1099-1112 em `provider.rs`
- **Categoria:** Segurança
- **Severidade:** Alta

```rust
fn redact_header_line(line: &str) -> String {
    let header_names = ["authorization", "x-api-key"];
    let bytes = line.as_bytes();
    let mut search_from = 0;
    let mut matches = Vec::new();
    while search_from < bytes.len() {
        let Some((index, header_len, value_start)) =
            find_sensitive_header(line, search_from, &header_names)
        else {
            break;
        };
```

**Descrição.** Duas falhas independentes:

1. **Allowlist insuficiente.** Apenas `authorization` e `x-api-key`. Ficam de fora `api-key`
   (usado pelo Azure OpenAI), `x-goog-api-key`, `cookie`, `set-cookie`, `proxy-authorization`,
   `x-auth-token` e `x-amz-security-token`.

2. **Não funciona em JSON.** `find_sensitive_header` exige que o caractere `:` apareça
   imediatamente após o nome do cabeçalher (linhas 571-573):

```rust
        if cursor >= bytes.len() || bytes[cursor] != b':' {
            continue;
        }
```

   Em um corpo de requisição JSON o conteúdo é `"authorization": "Bearer sk-..."`. Depois do nome
   vem `"`, não `:` — logo **nada é redigido**. Como as requisições para provedores são serializadas
   em JSON, o caso mais comum é justamente o que escapa.

Consequência: chaves de API em texto puro em logs, mensagens de erro e relatórios de diagnóstico.

**Correção sugerida.** (a) Ampliar a allowlist; (b) aceitar o separador `:` **ou** a sequência
`":` (com aspas), tratando o corpo JSON como cidadão de primeira classe; (c) preferencialmente,
redigir no nível da estrutura tipada (`serde_json::Value`) antes da serialização, em vez de
pós-processar texto — redigir strings é inerentemente frágil.

---

## Severidade MÉDIA

### M-01 — TOCTOU na escrita atômica do arquivo de credenciais

- **Arquivo:** `crates/slim-cli/src/oauth/store.rs`
- **Linhas:** 289-301
- **Categoria:** Segurança
- **Severidade:** Média

```rust
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| OAuthError::Store("auth temporary file creation failed".into()))?;
        let _cleanup = TemporaryFile(temporary.clone());
        drop(file);
        secure_auth_file(&temporary).map_err(|error| OAuthError::Store(error.to_string()))?;
        file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|_| OAuthError::Store("secured auth temporary file open failed".into()))?;
```

**Descrição.** O arquivo temporário é criado com `create_new(true)` (seguro) e **imediatamente
fechado** (`drop(file)`). A reabertura na linha 297 usa apenas `.write(true).truncate(true)` — sem
`create_new`, sem verificação de symlink. Entre o `drop` e a reabertura, um atacante local com
permissão de escrita no diretório pai pode remover o temporário e substituí-lo por um symlink: a
escrita seguinte trunca e sobrescreve o alvo do symlink com o JSON de credenciais.

A verificação de symlink existe (linhas 280-284), mas é feita **antes** da criação e sobre
`self.path`, não sobre `temporary`.

**Correção sugerida.** Não fechar e reabrir. Manter o handle retornado por `create_new` e escrever
nele; aplicar o DACL com `SetSecurityInfo` sobre o handle já aberto. Se a reabertura for
inevitável, usar `O_NOFOLLOW` (Unix) / `FILE_FLAG_OPEN_REPARSE_POINT` (Windows) e revalidar com
`symlink_metadata` **imediatamente antes** da escrita, aceitando que isso reduz mas não elimina a
janela.

---

### M-02 — DACL concede `FILE_ALL_ACCESS` a quatro entidades

- **Arquivo:** `crates/slim-cli/src/auth.rs`
- **Linhas:** 849-857
- **Categoria:** Segurança
- **Severidade:** Média

```rust
        let entries: Vec<EXPLICIT_ACCESS_W> = sids
            .iter()
            .map(|allowed| EXPLICIT_ACCESS_W {
                grfAccessPermissions: FILE_ALL_ACCESS,
                grfAccessMode: SET_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: trustee(&allowed.sid, allowed.trustee_type),
            })
            .collect();
```

**Descrição.** `FILE_ALL_ACCESS` inclui `DELETE`, `WRITE_DAC`, `WRITE_OWNER` e
`FILE_EXECUTE` — concedidos a *owner*, *usuário atual*, *SYSTEM* e *Administradores*. Para um
arquivo que contém apenas segredos estáticos (chaves de API, tokens OAuth), `FILE_EXECUTE` e
`WRITE_OWNER` são desnecessários e aumentam a superfície de abuso: qualquer processo rodando como
essas entidades pode, por exemplo, executar o arquivo ou transferir sua propriedade.

**Correção sugerida.** Reduzir a `FILE_GENERIC_READ | FILE_GENERIC_WRITE` (que já cobre leitura,
escrita e exclusão controlada). Administradores e SYSTEM podem tomar posse de qualquer forma, então
não há perda de funcionalidade — apenas eliminação de permissões que nunca são usadas.

---

### M-03 — SSRF e exfiltração de credencial no provedor ClinePass

- **Arquivo:** `crates/slim-core/src/provider/clinepass.rs`
- **Linhas:** 208-220
- **Categoria:** Segurança
- **Severidade:** Média

```rust
pub async fn fetch_clinepass_catalog(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Vec<ClinePassCatalogEntry>, ProviderError> {
    let mut response = client
        .get(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
```

**Descrição.** `url` é uma string arbitrária sem validação de esquema nem de host, e a chave de API
vai no cabeçalho `Authorization`. Se `url` for influenciável por configuração de workspace ou por
saída do modelo, a credencial é enviada a um host escolhido pelo atacante.

**Ponto positivo a registrar:** o tamanho da resposta **está** corretamente limitado — verificado
tanto via `content_length()` (linhas 224-228) quanto incrementalmente a cada chunk
(linhas 244-251), com `checked_add` contra overflow. Não há risco de exaustão de memória aqui.
O problema é estritamente o destino da requisição.

**Correção sugerida.** Validar `url` contra uma allowlist de esquema (`https` apenas) e de host
antes da requisição; rejeitar endereços IP literais e faixas privadas/link-local a menos que
explicitamente permitidos por configuração.

---

### M-04 — `taskkill.exe` sem caminho absoluto; `kill` no Unix não mata a árvore

- **Arquivo:** `crates/slim-core/src/process.rs`
- **Linhas:** 491-507
- **Categoria:** Segurança / Bug
- **Severidade:** Média

```rust
fn terminate_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
```

**Descrição.** Dois problemas distintos:

1. **Hijack de PATH.** Ambos os comandos são resolvidos pelo `PATH`/diretório atual pelo SO, ao
   contrário de todo o resto do codebase, que usa `ExecutableResolver` para obter caminhos
   absolutos (verificado: os demais `Command::new` recebem caminhos já resolvidos). Se o agente
   opera em um workspace não confiável que contenha um `taskkill.exe` na raiz, é esse que será
   executado.

2. **O nome mente no Unix.** `terminate_process_tree` promete matar a *árvore*, mas no Unix executa
   `kill -KILL <pid>`, que atinge apenas o processo direto. Processos-netos (comuns em shells e
   builds) sobrevivem ao timeout — vazamento de recursos. No Windows, `/T` faz o trabalho correto.

**Correção sugerida.** Resolver `taskkill.exe`/`kill` através do `ExecutableResolver` (ou embutir o
caminho absoluto do diretório de sistema). No Unix, criar o processo com `setsid`/process group e
enviar o sinal ao grupo (`kill -- -PGID`).

---

### M-05 — Ferramenta de shell executa PowerShell do modelo sem política

- **Arquivo:** `crates/slim-core/src/tools/shell.rs`
- **Linhas:** 117-133
- **Categoria:** Segurança
- **Severidade:** Média

```rust
    let program = powershell_program(runner)?;
    let request = ProcessRequest {
        cwd: cwd.to_path_buf(),
        program,
        args: [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
```

**Descrição.** Para ser preciso: **isto não é injeção de shell.** Os argumentos são passados
separadamente e não há interpolação de string em um interpretador — o Rust não usa shell.
O achado é diferente: **não há camada de política**. Qualquer comando produzido pelo modelo é
executado, sem allowlist, sem denylist, sem confirmação do usuário.

**Pontos positivos verificados:** timeout, token de cancelamento e orçamento de captura de saída
(`SHELL_CAPTURE_CAP_BYTES`) estão todos aplicados. A execução de comandos arbitrários é inerente a
um agente de codificação; a questão é se o usuário tem visibilidade e controle.

**Correção sugerida.** Introduzir uma camada de aprovação (confirmar comandos que casem com padrões
destrutivos: `rm -rf`, `git push --force`, `format`, `Remove-Item -Recurse`) e/ou um modo de
"somente leitura" que recuse a ferramenta. Registrar o comando completo em log auditável.

---

### M-06 — Mojibake no prompt de sistema enviado a todos os modelos

- **Arquivo:** `crates/slim-core/src/provider.rs`
- **Linhas:** 322, 326, 328, 330 (5 linhas afetadas)
- **Categoria:** Bug
- **Severidade:** Média

```rust
pub const NATIVE_SYSTEM_PROMPT: &str = r#"# CODING AGENT SYSTEM Ã¢â‚¬â€� v1.0 (compact)
```

**Descrição.** Confirmado por inspeção de bytes: a constante contém a sequência `Ã¢â‚¬â€\u{9d}`
(clássica dupla codificação UTF-8 de um travessão/em-dash), e não o caractere `—` (U+2014). Cinco
linhas do prompt carregam a mesma corrupção.

Consequências: desperdício de tokens em todas as requisições; fragmentação do tokenizer (sequências
de mojibake consomem mais tokens que o caractere original); e aparência de conteúdo corrompido caso
o prompt seja exibido ao usuário ou registrado.

**Correção sugerida.** Substituir as sequências por `—`. A causa provável foi um round-trip por uma
ferramenta que decodificou UTF-8 como Latin-1; verificar se o mesmo ocorreu em outros arquivos
(`crates/slim-core/src/provider.rs` foi o único afetado na varredura).

---

### M-07 — Envenenamento de lock não recuperável na fila de eventos

- **Arquivo:** `crates/slim-core/src/model.rs`
- **Linhas:** 84, 104, 135 (15 ocorrências no arquivo)
- **Categoria:** Bug
- **Severidade:** Média

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

**Descrição.** Este arquivo é o único do workspace que trata envenenamento de lock como fatal com
`.expect(...)`. O padrão adotado em todo o resto do código — verificado em `process.rs`,
`transport.rs`, `read.rs` e `store.rs` — é `unwrap_or_else(|poisoned| poisoned.into_inner())`.

O risco é de falha em cascata e **permanente**: se qualquer pânico ocorrer dentro de uma seção
crítica desta fila, o mutex fica envenenado e *todas* as chamadas subsequentes a `try_send` e
`send_interruptible` passam a panicar. Um único pânico transitório destrói o subsistema de eventos
pelo resto da vida do processo.

Observação de honestidade: os `expect("pending event")` das linhas 105, 114 e 125 parecem
genuinamente inalcançáveis (todo `event.take()` é seguido de `return`), então o gatilho mais
provável é outro pânico originado dentro da própria seção crítica.

**Correção sugerida.** Substituir por `unwrap_or_else(|poisoned| poisoned.into_inner())`, alinhando
ao padrão do repositório. Alternativamente, trocar `std::sync::Mutex` por `parking_lot::Mutex`, que
não tem o conceito de envenenamento.

---

### M-08 — `JsonLineFramer`: buffer ilimitado, custo quadrático e perda de mensagens

- **Arquivo:** `crates/slim-core/src/mcp/stdio.rs`
- **Linhas:** 9-21
- **Categoria:** Bug / Performance
- **Severidade:** Média

```rust
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, serde_json::Error> {
        self.buffer.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
            if trimmed.is_empty() {
                continue;
            }
            messages.push(serde_json::from_slice(trimmed)?);
        }
        Ok(messages)
    }
```

**Descrição.** Três defeitos em nove linhas:

1. **Sem teto de buffer (DoS).** Um servidor MCP que envie bytes sem nunca emitir `\n` faz
   `self.buffer` crescer sem limite até exaustão de memória. Servidores MCP são configurados pelo
   usuário e, na prática, são código de terceiros.

2. **Quadrático.** A cada iteração, `drain(..=index)` desloca o restante do buffer para o início e
   `position()` reescaneia do índice zero. Para um chunk com `k` linhas de comprimento total `n`, o
   custo é O(n·k). Entradas grandes de ferramentas (resultados de leitura, saídas de build) caem
   exatamente nesse caso.

3. **Perda de mensagens.** `serde_json::from_slice(trimmed)?` retorna `Err` no primeiro JSON
   malformado, mas as linhas anteriores já foram `drain`adas e as mensagens já parseadas são
   descartadas junto com o erro — o chamador não as recebe.

**Correção sugerida.** Manter um cursor de leitura em vez de `drain`; percorrer o buffer uma única
vez; impor um limite de tamanho de linha e descartar a linha (registrando o evento) quando excedido,
em vez de crescer indefinidamente; no erro de parse, retornar as mensagens já acumuladas junto com o
erro.

---

### M-09 — `expect()` em laço de eventos da TUI pode encerrar a interface

- **Arquivo:** `crates/slim-cli/src/tui.rs`
- **Linhas:** 1084 (e mais 62 ocorrências no mesmo arquivo)
- **Categoria:** Bug
- **Severidade:** Média

```rust
                    if projector.is_finished() {
                        let _ = projector.join();
                        let result = run.result.take().expect("pending result");
```

**Descrição.** `tui.rs` concentra **63** dos 122 `unwrap()`/`expect()` em código de produção do
workspace. O caso acima é representativo: a thread `projector` é finalizada e assume-se que
`run.result` foi preenchida. Se a thread projetora tiver pânico (o que `let _ = projector.join();`
descarta silenciosamente), `run.result` permanece `None` e o `expect` estoura **dentro do laço de
eventos da interface**, derrubando a TUI e perdendo o estado da sessão em andamento.

O padrão se repete em 1116, 1146, 1198, 1232 e dezenas de outros pontos: invariantes asseguradas
em outra parte do código são reafirmadas com `expect`, tornando qualquer divergência fatal para o
usuário.

**Correção sugerida.** Nos pontos em que o valor vem de uma thread de trabalho, tratar `None` como
estado de erro e reportar ao usuário (por exemplo, convertendo o pânico da projetora em
`Result<_, JoinError>` e propagando uma mensagem de falha de execução), em vez de panicar.

---

### M-10 — Lock adquirido a cada delta do stream de compactação

- **Arquivo:** `crates/slim-core/src/runtime/mod.rs`
- **Linhas:** 3618-3626, 3642-3649
- **Categoria:** Performance
- **Severidade:** Média

```rust
    let stream_result = client
        .stream_compaction_messages_cancellable(
            &[ProviderMessage::user(plan.prompt.clone())],
            cancellation_future,
            |event| {
                update_compaction_progress(&progress, &event);
                collected.push(event);
            },
        )
        .await;
```

```rust
fn update_compaction_progress(
    progress: &Arc<Mutex<CompactionAttemptProgress>>,
    event: &ProviderEvent,
) {
    let mut state = match progress.lock() {
```

**Descrição.** O callback é invocado **por evento do stream** — ou seja, por delta de texto
recebido via SSE. Cada delta adquire e libera o mutex. Em uma resposta longa de compactação isso
significa milhares de aquisições de lock para atualizar quatro booleanos, dos quais apenas o
primeiro de cada tipo tem efeito (`state.first_token_received = true`, etc.).

A corretude está preservada (a seção crítica não contém `await`), então o impacto é desperdício de
CPU e contenção com o leitor de progresso — não deadlock.

**Correção sugerida.** Usar `AtomicBool` para os quatro sinalizadores (`send_started`,
`headers_received`, `first_byte_received`, `first_token_received`), eliminando o mutex do caminho
quente. Alternativa: armazenar em `Cell`/buffer local e liberar para o estado compartilhado apenas
quando houver mudança de valor.

---

### M-11 — Lock compartilhado adquirido a cada chunk de 16 KiB nos pipes

- **Arquivo:** `crates/slim-core/src/process.rs`
- **Linhas:** 450-463
- **Categoria:** Performance
- **Severidade:** Média

```rust
        let mut progress = progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match kind {
            PipeKind::Stdout => progress.stdout_bytes = progress.stdout_bytes.saturating_add(read),
            PipeKind::Stderr => progress.stderr_bytes = progress.stderr_bytes.saturating_add(read),
        }
        if let Some(line) = String::from_utf8_lossy(&chunk[..read])
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
        {
            progress.last_line = bound_last_line(line);
        }
```

**Descrição.** O mutex é compartilhado entre as threads de `stdout` e `stderr` e é adquirido uma vez
por chunk de 16 KiB. Para um comando que produza centenas de megabytes, são dezenas de milhares de
aquisições concorrentes de um lock disputado.

Dentro da seção crítica há ainda duas alocações: `String::from_utf8_lossy` materializa um
`Cow` sobre os 16 KiB inteiros **apenas para extrair a última linha**, e `.lines().rev()` itera todo
o chunk. O progresso é amostrado a cada 1 segundo (`runtime`), então a utilidade é baixa.

Adicionalmente, a linha extraída pode ser parcial, já que a divisão por chunks não respeita
fronteiras de `\n`.

**Correção sugerida.** Acumular contadores localmente (sem lock) e publicar no estado compartilhado
no máximo a cada ~100 ms. Para `last_line`, varrer o chunk de trás para frente procurando `\n`
diretamente nos bytes, sem alocar, preservando um pequeno "resto" entre chunks para reconstruir a
linha corretamente.

---

## Severidade BAIXA

### B-01 — Preflight do governor aloca para processar um único item

- **Arquivo:** `crates/slim-core/src/runtime/governor.rs`
- **Linhas:** 277-291
- **Categoria:** Bug / Performance
- **Severidade:** Baixa

```rust
        self.observe_before_batch(
            cwd,
            vec![(
                native_spec,
                batch_id.to_owned(),
                call_id.to_owned(),
                tool_name.to_owned(),
                arguments.to_owned(),
            )],
        )
        .await
        .pop()
        .expect("single governor preflight")
```

**Descrição.** Para processar **um** tool call, aloca-se um `Vec` e quatro `String` (`to_owned()`)
somente para reutilizar o caminho de lote — inclusive `arguments`, que é tipicamente a maior string
da requisição. Depois, `.pop().expect(...)` introduz um ponto de pânico que depende de
`observe_before_batch` preservar a cardinalidade da entrada.

**Correção sugerida.** Extrair o corpo de `observe_before_batch` para uma função que processe um
único item e fazer o caminho de lote chamá-la em laço; o caso unitário passa a alocar nada e o
`expect` desaparece.

---

### B-02 — Dez `expect()` apoiados em invariante não-local

- **Arquivo:** `crates/slim-core/src/session/queue.rs`
- **Linhas:** 206, 224, 239, 257, 273, 380, 385, 409, 428, 453
- **Categoria:** Estilo
- **Severidade:** Baixa

```rust
    pub fn claim(&mut self, operation_id: &str) -> Result<QueueItem, DurableQueueError> {
        let entry = self.entry(operation_id)?;
        if entry.status != QueueStatus::Queued {
            return Err(self.transition_error(operation_id));
        }
        self.remove_pending(operation_id);
        let entry = self
            .entries
            .get_mut(operation_id)
            .expect("queue entry checked before mutation");
```

**Descrição.** Verificado que o `expect` é seguro hoje: `self.entry(...)?` (linha 198) garante a
existência da entrada, e `remove_pending` opera sobre a lista de pendências, não sobre `entries`.
O problema é de manutenção: a segurança depende de uma invariante estabelecida em outra função. Se
alguém fizer `remove_pending` também remover de `entries`, esses dez pontos passam a panicar — e
trata-se do caminho de durabilidade da fila, onde um pânico corrompe a sessão.

**Correção sugerida.** Reestruturar para que `entry()` devolva o `&mut QueueEntry` já validado,
eliminando a segunda busca. Onde isso não for possível, propagar `DurableQueueError` em vez de
`expect`.

---

### B-03 — `expect()` em catálogos estáticos e estados de capacidade

- **Arquivo:** `crates/slim-core/src/session/capabilities.rs`
- **Linhas:** 144, 147, 938, 964, 966, 1013 (11 no arquivo)
- **Categoria:** Estilo
- **Severidade:** Baixa

```rust
        .expect("static native tool catalog");
```

**Descrição.** Os dois primeiros operam sobre catálogos estáticos e são razoáveis (falha só se a
inicialização estática tiver falhado). Os demais (`938`, `964`, `966`, `1013`) seguem o mesmo padrão
de B-02: reafirmam com `expect` um estado verificado em outro lugar ("capability state checked
before claim").

**Correção sugerida.** Manter `expect` nos catálogos estáticos (documentando por que é inalcançável)
e converter os de estado dinâmico para propagação de erro.

---

### B-04 — Cache de leitura invalidado apenas por `(tamanho, mtime)`

- **Arquivo:** `crates/slim-core/src/tools/read.rs`
- **Linhas:** 42-45, 71-80
- **Categoria:** Bug
- **Severidade:** Baixa

```rust
struct FileVersion {
    len: u64,
    modified: Option<SystemTime>,
}
```

```rust
        let version = FileVersion {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
```

**Descrição.** A identidade do conteúdo é inferida de tamanho + data de modificação, o que não é
uma identidade: uma edição que preserve o tamanho e caia na mesma marca de tempo (ou que ajuste o
`mtime` deliberadamente) faz o cache servir conteúdo obsoleto. Não há `ctime`, inode nem hash.

Impacto limitado: afeta os *checkpoints* de linha usados para paginação. No pior caso, o agente lê
uma versão desatualizada de um arquivo que acabou de ser modificado por uma ferramenta —
exatamente o cenário em que a leitura correta mais importa.

**Correção sugerida.** Incluir o `mtime` com resolução completa (atuais APIs truncam em alguns
sistemas de arquivos) e, preferencialmente, invalidar o cache de um arquivo sempre que uma
ferramenta de escrita/patch o tocar, em vez de confiar em metadados.

---

### B-05 — `from_utf8_lossy` a cada chunk apenas para extrair a última linha

- **Arquivo:** `crates/slim-core/src/process.rs`
- **Linhas:** 457-463
- **Categoria:** Performance
- **Severidade:** Baixa

**Descrição.** Já descrito em M-11, listado separadamente porque tem correção independente: mesmo
após eliminar o lock (M-11), resta o custo de materializar um `Cow<str>` de 16 KiB por chunk só
para descobrir a última linha não vazia.

**Correção sugerida.** Varrer os bytes de trás para frente procurando `\n` e aplicar
`String::from_utf8_lossy` apenas no pequeno trecho final.

---

### B-06 — Polling de 1 ms quando a fila de eventos está cheia

- **Arquivo:** `crates/slim-core/src/model.rs`
- **Linhas:** 138-145
- **Categoria:** Performance
- **Severidade:** Baixa

```rust
            let (mut next, _) = self
                .queue
                .not_full
                .wait_timeout(state, Duration::from_millis(1))
                .expect("event queue wait");
```

**Descrição.** O `wait_timeout` de 1 ms transforma um bloqueio por condição em *busy-wait* com
granualidade de milissegundo quando a fila satura. Em um laço de agente que emite muitos eventos,
isso desperta milhares de vezes por segundo, consumindo CPU e medindo `send_wait_duration` de forma
enganosa (o valor passa a refletir o polling, não a espera real).

**Correção sugerida.** Usar `wait` (sem timeout) e garantir que o consumidor notifique `not_full`
ao drenar; ou, se o timeout for necessário para o cancelamento, elevá-lo para algo como 50–100 ms e
verificar o cancelamento entre esperas.

---

### B-07 — Três aquisições de lock e um `stat` por resolução de executável

- **Arquivo:** `crates/slim-core/src/process.rs`
- **Linhas:** 67-99
- **Categoria:** Performance
- **Severidade:** Baixa

```rust
        let generation = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation;
```

**Descrição.** Cada chamada a `resolve()` trava o mutex três vezes (ler `generation`, consultar o
cache, inserir o resultado) e, em caso de acerto, executa um `stat` para validar `is_file()`
(linha 84). Entre a liberação (linha 91) e a nova aquisição (linha 94) ocorre I/O
(`search_path`), o que é correto para não segurar o lock, mas permite trabalho duplicado sob
concorrência.

**Correção sugerida.** Reduzir a duas aquisições lendo `generation` junto com a consulta de cache.
O `stat` por acerto só é útil se o cache puder ficar obsoleto — nesse caso, validar com um TTL em
vez de a cada acesso.

---

### B-08 — `search_path` não verifica permissão de execução no Unix

- **Arquivo:** `crates/slim-core/src/process.rs`
- **Linhas:** 155-163
- **Categoria:** Bug
- **Severidade:** Baixa

```rust
fn search_path(program: &OsStr, path: &OsStr, path_ext: &OsStr) -> Option<PathBuf> {
    let candidates = executable_names(program, path_ext);
    std::env::split_paths(path).find_map(|directory| {
        candidates.iter().find_map(|name| {
            let candidate = directory.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}
```

**Descrição.** `find_map` retorna o primeiro *arquivo* encontrado e interrompe a busca. Se o
primeiro diretório do `PATH` contiver um arquivo homônimo sem permissão de execução, a busca para
ali e o `spawn` falha com "permission denied", em vez de continuar procurando nos diretórios
seguintes — comportamento diferente do `execvp` padrão.

**Ressalva de honestidade:** o projeto é fortemente centrado em Windows (a autenticação usa
`USERPROFILE` e ACLs nativas; `auth_file_path` retorna `None` sem `USERPROFILE`), então este
caminho Unix é pouco exercitado. Registrado como baixa por isso.

**Correção sugerida.** No Unix, exigir que o candidato seja arquivo **e** executável
(`std::os::unix::fs::PermissionsExt::mode() & 0o111 != 0`) antes de aceitá-lo.

---

### B-09 — `lock_auth_store` é inócuo fora do Windows

- **Arquivo:** `crates/slim-cli/src/auth.rs`
- **Linhas:** 429-433
- **Categoria:** Segurança
- **Severidade:** Baixa

```rust
    #[cfg(not(windows))]
    {
        let _ = parent;
        Ok(AuthStoreLock {})
    }
```

**Descrição.** Em plataformas não-Windows não há exclusão mútua entre processos: devolve-se um
guard vazio. Somado a `secure_auth_file` (linhas 498-501), que retorna
`Err(AuthError::UnsafePermissions)` fora do Windows, o armazenamento de credenciais é
funcionalmente inoperante nessas plataformas — o que reduz bastante o impacto prático.

Ainda assim, se `USERPROFILE` estiver definido em um ambiente Unix, `auth_file_path` (linha 484)
passa a devolver um caminho e a escrita ocorre **sem trava e sem verificação de permissões**.

**Correção sugerida.** Implementar a trava com `flock`/`fcntl` no Unix, ou recusar explicitamente o
armazenamento em arquivo fora do Windows em vez de retornar silenciosamente um guard vazio.

---

# Achados não verificados (varredura automatizada)

Os itens abaixo vieram de varredura automatizada sobre `tools/`, `context/`, `provider*`, `mcp/` e
`lsp/`. **Não foram confirmados por leitura direta** e por isso estão fora das contagens do resumo
executivo. Servem como fila de trabalho para uma próxima rodada.

| Referência | Possível achado |
|---|---|
| `context/compact.rs:523-538` | Busca binária refaz `bounded_transcript` O(n) ~18× por turno (O(n·log n)) |
| `context/compact.rs:353-394` | `estimate_provider_message_tokens` varre o transcript inteiro; custo real O(n·k), não O(n²) |
| `context/compact.rs:180-184` | `take_prepared` re-hasha (SHA-256) o prefixo a cada turno |
| `context/compact.rs:555` | Usa `CompactionPolicy::default()` em vez da policy ativa |
| `context/compact.rs:676` | Slice `[..16]` sem verificar `is_char_boundary` |
| `provider.rs:1795-1905` | `canonical_messages` copia o transcript inteiro apenas para hashear |
| `provider.rs:2667, 2926` | Serializar → re-parsear → re-serializar o corpo da requisição |
| `provider.rs:507-517` | `cache_namespace()` reserializa o corpo a cada requisição |
| `provider.rs:1035` | `cache_key.map_or(0, String::capacity)` como orçamento inicial |
| `provider.rs:2083-2127` | `normalize_base64` aceita padding no meio (`AB=C`) e reemite payload corrompido |
| `provider/codex.rs:32-39` | `unreachable!()` em caminho alcançável |
| `tools/rg_search.rs:101-156` | Sem timeout de wall-clock no `rg` quando `cancellation` é `None` |
| `tools/rg_search.rs:190-200` | `parse_rg_line` com fallback frágil por `:` |
| `tools/search.rs:368` | `flatten()` descarta erros de travessia → busca silenciosamente incompleta |
| `tools/search.rs:386-393` | Detecção de binário examina apenas os primeiros 8 KiB |
| `tools/code_intel.rs:107-131` | Fallback léxico não resolve symlink (complementa A-01) |
| `mcp/http.rs:1-3` | `authorize_http` sem comparação de tempo constante; token vazio produz `"Bearer "` |
| `mcp/mod.rs:11-13` | `canonical_name` sem sanitização → colisão `a`+`b.c` vs `a.b`+`c` |
| `slim-lsp/src/document.rs:121,126,158` | Três `expect()` apoiados em verificações imediatamente anteriores |
| `slim-lsp/src/instance.rs:43` | `parse().expect("file url parses as lsp Uri")` sobre URL derivada do workspace |

---

# Verificações negativas (o que está correto)

Registrar o que foi examinado e aprovado evita retrabalho em auditorias futuras:

- **PKCE e estado OAuth.** `oauth/pkce.rs` usa 32 bytes de `getrandom` para o verificador e 24 bytes
  para o `state`, com desafio S256 (SHA-256, base64url sem padding). Correto.
- **Tratamento de envenenamento de lock.** Fora de `model.rs` (M-07), o padrão
  `unwrap_or_else(|poisoned| poisoned.into_inner())` é aplicado de forma consistente em
  `process.rs`, `transport.rs`, `read.rs` e `store.rs`.
- **Injeção de shell.** Nenhuma interpolação de string em `cmd /C`, `sh -c` ou `powershell -Command`
  foi encontrada. Todos os `Command::new` (exceto M-04) recebem caminhos absolutos do
  `ExecutableResolver`.
- **Mutação de ambiente.** Nenhuma em código de produção — todas confinadas a `#[cfg(test)]`.
- **Limites de rede e de tamanho.** Timeouts de HTTP/SSE e tetos de resposta estão aplicados de
  forma consistente, inclusive com `checked_add` contra overflow em `clinepass.rs`.
- **Slices UTF-8.** As operações de fatiamento por índice verificam `is_char_boundary` na maior
  parte do código.
- **Redação de segredos em `Debug`.** `Pkce` e `ProviderAuth` implementam `Debug` com `[REDACTED]`.
- **Resolução de executáveis.** O design de pré-resolver caminhos absolutos antes de `spawn` é
  superior à prática habitual e bloqueia hijack por diretório atual na maior parte do código.
