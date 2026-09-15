# Harness Slim — etapa 1: simplicidade interna

Data: 2026-09-04. Checkout: `D:\Slim`. Estado: implementação concluída. A evidência original foi preservada; a correção posterior de tok/s tem gate próprio no fim deste registro.

## Intenção e limites

Simplificar caminhos realmente usados sem importar arquitetura ou buscar paridade com o Pit. Preservar CLI, TUI, protocolos dos providers, formatos persistidos, redaction, limites e alterações preexistentes. Aceitação: redução concreta de complexidade, correção reproduzida quando aplicável, testes existentes relevantes e gates do projeto, sem chamadas pagas.

Fora desta etapa: novas funcionalidades, redesenho visual, migração de sessões, mudanças de provider/modelo, infraestrutura de testes, benchmark de latência comercial e refatoração geral por tamanho de arquivo. Não houve subagentes, commit, reset, troca de branch ou limpeza do checkout.

## Base e autoria das alterações

O checkout já tinha mudanças em 42 arquivos rastreados e sete arquivos não rastreados, incluindo xAI/OAuth, limites do loop, tools e TUI. Esse trabalho não é resultado desta etapa. O patch inicial e cópias dos arquivos tocados foram guardados em `%TEMP%\slim-harness-etapa1-20260904` para comparar com a entrada, não apenas com `HEAD`.

Alterações de código desta etapa:

- `crates/slim-core/src/runtime/mod.rs` — estado do streaming.
- `crates/slim-cli/src/headless.rs` — execução, limites e histórico.
- `crates/slim-cli/src/tui.rs` — consumo dos limites, com ajuste da fixture interna.
- `crates/slim-core/tests/runtime_abort.rs` — uma regressão de descarte do future.

Comparação com a entrada: 83 linhas acrescentadas e 163 removidas nesses quatro arquivos, redução líquida de 80 linhas, incluindo testes e formatação dos arquivos tocados. Nenhuma dependência ou abstração de arquitetura nova. A comparação dos demais diffs de código com o patch de entrada não encontrou alterações desta etapa.

## Caminhos examinados

| Caminho | Wiring confirmado no código atual | Consequência para a revisão |
|---|---|---|
| Entrada CLI | `slim-cli/main.rs` seleciona `run_tui` ou `run_cli`; `cli.rs::parse_cli_args` é também usado na preparação da TUI | Não remover opções ou rotas só pela contagem de referências. A detecção de stdin em `main.rs` é uma segunda interpretação parcial dos argumentos; fica como investigação delimitada abaixo. |
| Headless | `run_provider_headless*` → `execute_provider_turn` → helper de LSP local → `execute_provider_turn_async` | Caminho apropriado para eliminar wrappers repetidos, com preservação da fronteira síncrona/Tokio. |
| TUI | `start_active_run` → execução async ou resume durável → projector → `UiEvent::from_core` → `Action::UiEventReceived` → reducer | Não existe um segundo agent loop próprio da TUI. As diferenças de projeção, paginação e interação têm responsabilidade concreta. |
| Runtime/provider | `run_agent_loop_with_messages` → request preparado/context snapshot → `run_provider_messages_with_tools_after_snapshot` → `HttpProviderClient::stream_prepared_cancellable` | O estado temporário do `AppHandle` era dispensável e vulnerável ao descarte do future. |
| Sessão retomada | preflight → histórico/checkpoints → `open_resume_v2` → `drive_manual_async` → executor → persistência do resultado | O driver persiste prefixo e terminal; não é apenas um wrapper de HTTP. Preservadas as restrições diferentes de interação no headless e TUI. |
| Providers | dispatch concreto em `headless.rs`; trait e requests preparados em `provider.rs`; reutilização de wire em Command Code e xAI | Reuso já existe. Não trocar adapters por uma nova enum/trait object só para encurtar o `match` da CLI. |
| Tools e estado da UI | registro nativo compartilhado entre turnos; `ContentStore` paginado; fila de eventos com backpressure; projeção dos eventos de execução | Esses estados têm ciclos de vida diferentes. Não eliminar snapshots, fila ou conteúdo completo confundindo-os com previews. |

Esta é uma revisão dos fluxos centrais, não uma alegação de leitura exaustiva de cada arquivo do harness. LSP interno, todos os adaptadores e todos os estados duráveis não receberam auditoria completa nesta etapa.

## Mudanças implementadas e evidência

### S1 — streaming usa diretamente o estado do runtime

**Verificado.** Antes, `run_provider_messages_with_tools_after_snapshot` substituía `self.app` por `AppHandle::fake()`, mantinha o estado real numa variável local através de um `.await` e restaurava-o ao terminar. Descartar o future enquanto pendente impedia a restauração e perdia os eventos acumulados no runtime.

Agora o normalizador recebe `&mut self.app` diretamente no callback e na finalização. Foram removidos o estado substituto e a obrigação de restaurá-lo. O estado permanece no runtime mesmo se a operação assíncrona for descartada. Isso não sintetiza um evento terminal nem muda o protocolo de cancelamento cooperativo.

Regressão `dropping_a_pending_provider_future_preserves_app_state`, em `runtime_abort.rs`: endpoint localhost, primeiro poll pendente, descarte do future, verificação do `ContextSnapshot` já emitido. Sem sleeps, provider real ou nova infraestrutura. A suíte existente cobria cancelamento via token, mas não descarte do future; esse teste falhou antes da correção e passou depois.

Limite de impacto: a perda foi reproduzida na API do runtime. Não foi demonstrado um incidente visual em console físico nem uma perda de sessão de usuário causada por esse cenário.

### S2 — uma única gestão do LSP temporário

**Verificado.** Dois helpers async repetiam anexação condicional do gerenciador, chamada ao mesmo executor e shutdown do gerenciador criado localmente. Foram reunidos em `execute_provider_turn_with_local_lsp`, recebendo o path opcional, eventos e rota de interação já existentes. Um wrapper síncrono intermediário sem decisão própria foi removido.

Mantidos: o runtime Tokio compartilhado do headless; o LSP fornecido pelo host não é encerrado pelo helper; shutdown do LSP local ocorre após retorno normal ou erro da execução. A TUI comum continua usando seu gerenciador persistente. Não foi acrescentada uma garantia nova de shutdown assíncrono caso esse próprio helper seja descartado.

### S3 — limites viajam no tipo que já existia

**Verificado.** `ToolLoopLimits` era desmontado em cinco campos de `ProviderExecution` e reconstruído em `tui.rs::run_stop_message`. `ProviderExecution` agora retém `limits: ToolLoopLimits`, consumido diretamente pela TUI. São tipos internos à crate; formato JSONL público, configuração, valores padrão e texto de parada permanecem iguais.

### S4 — menos cópia e construção duplicada

**Verificado.** O executor já recebia `ProviderRunOptions` por valor, mas clonava todo `options.history` antes de acrescentar o prompt. Agora move esse vetor. Isso elimina uma cópia profunda por execução nesse ponto, incluindo conteúdo e anexos do histórico, sem eliminar as cópias necessárias ao host ou ao runtime. Não houve benchmark para quantificar ganho de tempo/RAM.

A resposta de input vazio também usa o `input_required_result` existente, já utilizado pelo resume, em vez de manter outra construção manual dos mesmos campos.

### S5 — remoção de telemetria fictícia nos testes

**Verificado.** `provider_cache_report_line` tinha `#[cfg(test)]` e era chamada somente por seus dois testes. Não era compilada no produto e não testava um ponto de emissão real de stderr. Função e dois testes foram removidos. Isso não remove `ProviderCache`, compartilhamento de transporte, estatísticas ou testes de cache no core. O caminho normal continua sem replay local de respostas.

## Decisões de preservação

- **Adapters concretos:** o `match` de providers repete a chamada ao loop, mas cada braço constrói um tipo diferente e preserva diferenças de auth, reasoning e saída. Unificá-lo exigiria mexer na fronteira genérica ou adicionar despacho; benefício insuficiente nesta etapa.
- **Requests preparados:** nos caminhos lidos de xAI e Command Code já há preparação com `Value` e reaproveitamento dos adapters de wire. A existência de métodos antigos que retornam `HttpRequest` não demonstra que o hot path serializa e desserializa repetidamente. Não há uma otimização adicional comprovada aqui.
- **Sessões e projeção:** persistência de prefixo/resultado, checkpoints, eventos causais e previews da UI não são cópias intercambiáveis. Permaneceram intactos.
- **Tool registry compartilhado:** preservado; o teste existente `list_cursor_survives_the_next_tui_prompt` demonstra a necessidade de continuidade entre turnos.
- **Formato e layout:** nenhuma decisão normativa da TUI alterada; diferenças de rustfmt preexistentes fora dos arquivos tocados não foram corrigidas oportunisticamente.

## Validação desta sessão

- Base: `cargo test -p slim-core --test runtime_abort` → 7 passed, 0 failed.
- RED: mesma suíte filtrada por `dropping_a_pending_provider_future_preserves_app_state` → 0 passed, 1 failed, exit 101; assert de retenção do snapshot.
- GREEN: `cargo test -p slim-core --test runtime_abort` → 8 passed, 0 failed, exit 0.
- Focados: `cargo test -p slim-cli --lib --test provider_cli --test tui_bridge --test headless_resume --test headless_contract` → 130 passed, 0 failed, exit 0; contagens 84 + 10 + 20 + 8 + 8.
- `cargo clippy --workspace --all-targets -- -D warnings` → exit 0.
- Toolchain verificado: rustc `1.98.0 (88d9e12ae 2026-08-18)`, caminho Scoop/MSVC usado pelo script local.
- `cargo fmt --all -- --check` → exit 1 por drift preexistente fora dos quatro arquivos tocados, em OAuth/xAI, testes TUI e trechos LSP/TUI. Os arquivos desta etapa foram formatados com `rustfmt --edition 2021 --config skip_children=true`.

- Gate integral `cargo test --workspace` → **1096 passed, 0 failed, 1 ignored, 88 suítes, 0 compiler warnings**, exit 0. Contagem calculada somando as linhas `test result: ok` do log desta sessão, incluindo doc-tests. O único ignored continua sendo o gate físico ConPTY; nenhum `--skip` foi usado. Foram removidos dois testes de código exclusivo de teste e acrescentada uma regressão real.
- `.\refresh-slim.ps1 -Test` → testes integrais, build release e instalação concluídos, exit 0. Saída: `OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 18:51:23)`.
- `target/release/slim.exe` e `C:\Users\User\bin\Slim.exe`: **14.926.848 bytes**, SHA-256 **`9F439A59BD121A2F3402965FC94268D02226B440469352CE2E75A23ABB1FB37A`**, idênticos nesta sessão.
- Binário instalado: `--version` → `slim 0.1.0`, exit 0; `--headless --fake "etapa 1 offline"` → `success`, exit 0.
- `rustfmt --edition 2021 --config skip_children=true --check` nos quatro arquivos da etapa → exit 0.
- `git diff --check` dos arquivos desta etapa → exit 0. Global → exit 2, exclusivamente por `crates/slim-tui/tests/layout_golden.rs:635: new blank line at EOF`. O diff desse arquivo é idêntico ao patch de entrada; não foi alterado para limpar o gate global.
- Logs locais auxiliares: `%TEMP%\slim-harness-etapa1-20260904\workspace-tests.log`, `refresh.log`, `fmt-workspace.log` e diffs da etapa. A evidência necessária à continuidade está resumida aqui, sem depender da retenção desses temporários.

### Verificação de entrega — RULES.md §4

- [x] Rodei o que afirmo ter rodado; comandos, resultados e saída `OK:` registrados acima.
- [x] Números citados contados nesta sessão; comparação com a entrada separada do diff contra `HEAD`.
- [x] Caminhos/símbolos de evidência lidos nesta sessão.
- [x] `cargo test --workspace` verde: 1096 passed, 0 failed, 1 ignored, 0 warnings.
- [x] `.\refresh-slim.ps1 -Test` executado, imprimiu `OK:`; hashes target/PATH conferidos.
- [x] Docs de status e índice atualizados. Referências correntes à contagem anterior substituídas; números de gates históricos no §7 do tracker preservados como histórico, não como validação atual.
- [x] Incertezas e limitações declaradas: fmt/diff globais preexistentes, console físico e providers comerciais não validados, sem benchmark de desempenho.

Não aplicáveis: revisão de contrato visual, regeneração do ZIP de distribuição, chamadas comerciais e nova infraestrutura de testes. O deploy é o executável local exigido pelo projeto.

## Continuidade: começar daqui

1. Revalidar `git status` e ler `AGENTS.md`/`RULES.md`. Não atribuir a esta etapa todas as mudanças contra `HEAD`; usar a lista de autoria acima.
2. **Hipótese delimitada:** estudar a interpretação de argumentos em `main.rs` (`has_positional_prompt`/`known_options_without_prompt`) versus `cli.rs::parse_cli_args`. Critério para mudança: uma única decisão sobre stdin/validação, preservando erros rápidos sem bloquear em stdin aberto, `--recover`, prompts e flags. Não há bug desse caminho demonstrado por esta etapa; não foi implementada solução especulativa.
3. **Limitação conhecida:** o helper de LSP conserva a semântica de shutdown anterior e não garante cleanup async quando o próprio future é descartado. Só ampliar esse trabalho após reproduzir um processo órfão ou outro efeito concreto.
4. A auditoria de todos os detalhes de sessions/capabilities e do LSP permanece disponível para uma etapa futura; não há recomendação de removê-los em bloco. Mapear callers de produto, API pública e contratos antes de concluir desuso.
5. Não repetir a proposta de criar um adapter universal ou um novo gerenciador de execução: esta etapa decidiu que o custo não se justifica. Não retestar provider pago sem autorização.

Nenhuma chamada paga foi executada. A validação é offline/localhost; console físico, autenticação real e qualidade das respostas comerciais não foram avaliados.


## Correção posterior — indicador tok/s

Solicitada e concluída em 2026-09-04. Esta seção registra apenas a correção autorizada após a revisão do exibidor; os números anteriores documentam o checkpoint original da etapa 1.

**Verificado — causa e comportamento:** a taxa preferia o tempo de consumo dos eventos na UI ao tempo do runtime. Eventos acumulados inflavam a velocidade; requests seguintes sem texto podiam reutilizar a duração anterior. O fallback por caracteres aparecia sem marca de estimativa.

A implementação remove os três campos de cronômetro/duração da UI. Calcula uma vez no `RequestCompleted`, com `output_tokens / ((provider_latency_ms - FirstSemantic.elapsed_ms) / 1000)`, e limpa a taxa ao iniciar outro request. Usa os tokens finais do provider quando disponíveis; caso contrário, conserva o estimador por caracteres existente e mostra `~` nos formatos completo e compacto. Ausência de duração válida ou overflow de usage oculta a taxa. O indicador representa a média observada desde o primeiro evento semântico até concluir o request, incluindo o transporte nesse intervalo; não mede decode puro do servidor nem velocidade instantânea durante o streaming.

**Escopo:** `crates/slim-tui/src/app.rs`, `runtime.rs`, `view_model.rs` e `crates/slim-tui/tests/layout_golden.rs`. Nenhuma alteração ao runtime core, providers, formatos persistidos ou dependências. Um teste existente ampliado e um teste novo, determinísticos, sem sleeps ou infraestrutura. Alterações preexistentes e concorrentes foram preservadas; comparação dos hashes de todos os fontes Rust após o gate não encontrou drift. A etapa 2 tem [registro separado](HARNESS-SLIM-ETAPA-2.md).

**Evidência desta correção:**

- RED: `cargo test -p slim-tui --test layout_golden footer_` → 4 passed / 2 failed, exit 101. Atraso sintético da UI produzia taxa errada, e o fallback não tinha `~`.
- GREEN: `cargo test -p slim-tui --test layout_golden` → 28 passed / 0 failed. Os casos agora dão 85 tok/s para 170 tokens em 2 s e 20 tok/s para o request seguinte de 200 tokens em 10 s; prefixo estimado e tempo inválido conferidos.
- `cargo clippy --workspace --all-targets -- -D warnings` → exit 0. `rustfmt --edition 2021 --config skip_children=true --check` nos quatro arquivos → exit 0. `git -c core.safecrlf=false diff --check` global → exit 0.
- `refresh-slim.ps1 -Test` executou `cargo test --workspace`, sem filtro: **88 suítes / 1100 passed / 0 failed / 1 ignored / 0 compiler warnings**; release e instalação concluídos, exit 0.
- `OK: Slim slim 0.1.0 implantado em C:\Users\User\bin\Slim.exe (build de 09/04/2026 19:19:01)`.
- Target e PATH: **14.942.720 bytes**, SHA-256 **`18FFF12EE3D45FE4D6DF95D8EA67ECE4B65321CA359F7F3239C5A6E9830C7695`**, idênticos. `Slim.exe --version` → `slim 0.1.0`; `Slim.exe --headless --fake 'tok/s offline'` → `success`; ambos exit 0.

**Limitações e continuidade:** sem chamadas pagas; precisão observada em provider comercial e console físico não foram exercitados. O teste ConPTY físico continua ignorado. Sem regeneração do ZIP de distribuição. Não se fez formatação global: o drift preexistente fora dos quatro arquivos não pertence à correção. Não há pendência de implementação identificada no escopo; se for necessário avaliar decode do servidor, distinguir primeiro essa métrica da média atual e usar evidência própria do provider.

### Verificação de entrega da correção — RULES §4

- [x] Comandos e saídas executados nesta sessão registrados acima.
- [x] Contagens atuais somadas da saída integral, sem reaproveitar gates anteriores.
- [x] Arquivos citados lidos; nenhuma referência numérica de linha sem leitura.
- [x] `cargo test --workspace`: 1100 passed / 0 failed / 0 warnings; 1 ConPTY físico ignored.
- [x] `refresh-slim.ps1 -Test` executado e `OK:` registrado.
- [x] Status, checkpoint, tracker e índices atualizados; referências correntes antigas removidas, checkpoints históricos preservados.
- [x] Limitações declaradas; provider pago, console físico e ZIP são não aplicáveis à entrega local desta correção.
