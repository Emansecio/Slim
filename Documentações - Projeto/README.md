# Documentação do Slim — índice canônico

> **Status de implementação: checkpoint de integração parcial; v1 ainda não
> concluída.** Atualizado em: 2026-08 (execução do tracker de auditoria TUI).
>
> Headless e TUI fullscreen usam provider, agent loop e tools reais. `Slim` abre
> a TUI normal mesmo deslogado; `/login` conecta OAuth nativo Claude Pro/Max ou
> ChatGPT Plus/Pro. `Slim --headless` seleciona a CLI headless. Na TUI: reducer
> único normativo, scrollback virtualizado/navegável, paleta §21.3 estratificada,
> lanes bounded com coalescer real, tool blocks tipados, rails operacionais,
> composer boxed, command palette, motion básico, proptest/fault/golden e bench
> com gate (p95 ~2 ms ≤ 16 ms). Pendente: gate físico Windows (PTY/console real),
> Todo/Plan/Goal end-to-end, inspectors/search/clipboard/imagem.

Este índice aponta o contrato, o plano e a evidência da versão `0.1.0`; não
declara que todos os contratos já estejam integrados.

## Matriz de implementação atual

| Área | Estado |
|---|---|
| Headless | Integrado: providers OpenAI-compatible/Anthropic SSE, tools nativas, modos, auth DACL, imagem local, compaction, usage, artifacts e anti-loop. |
| TUI | Integrada ao mesmo provider/loop/tools: composer, stream incremental com coalescer em lanes bounded, lifecycle, tool blocks tipados agregados, usage, modos, cancelamento, scroll virtualizado pin/live-edge, paleta §21.3 estratificada, rails operacionais, composer boxed, command palette, markdown-light e restauração fullscreen; golden matrix via TestBackend; PTY E2E físico pendente (`#[ignore]`, requer console real); sem provider live nos gates. |
| Cache | Implementado/testado, mas inativo no caminho normal: construção usa `HttpProviderClient::new`. |
| Sessões | Writer/recovery/branch e `--session` existem; sem resume, seleção de recovery ou branch/fork na CLI/TUI. |
| Skills/MCP | Catálogo, framing, auth/lifecycle e invocation têm módulos/testes; startup/loop/catálogo de tools não os conectam. |
| Subagentes | Contratos de scheduler/activity/child-session têm testes; não há child agents reais. |
| Todo/Plan/Goal | Estruturas/testes existem, mas não são tools vivas end-to-end; Plan retorna `approval_required` antes de gerar plano via provider. |
| Release/validação | Release determinístico/hash-valid do checkpoint; matriz física de terminal continua não verificada. |

## Ordem de leitura

0. [`../AGENTS.md`](../AGENTS.md) e [`../RULES.md`](../RULES.md) — regras
   obrigatórias para agentes (deploy do binário, anti-alucinação, incidentes).
1. [DECISOES-GRILL-PRE-IMPLEMENTACAO.md](DECISOES-GRILL-PRE-IMPLEMENTACAO.md) —
   decisões normativas e limites da v1.
2. [PLANO-IMPLEMENTACAO.md](PLANO-IMPLEMENTACAO.md) — implementação e gates,
   com o estado final registrado ao fim.
3. [RUST-CLI.md](RUST-CLI.md) — mapa de arquitetura e paridade.
4. [DESIGN-SLIM-TUI.md](DESIGN-SLIM-TUI.md) — contrato visual/funcional da TUI.
5. [VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md](VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md)
   — riscos e POCs.
6. [POC-RESULTS.md](../POC-RESULTS.md) — evidência executada e limitações.
7. [AUDIT-SLIM-TUI-TRACKER.md](AUDIT-SLIM-TUI-TRACKER.md) — auditoria da TUI vs
   spec e tracker vivo de execução (slices A/B/C/L, gates e log).

Quando houver conflito, prevalecem as decisões finais, depois este plano e os
contratos específicos da TUI. Pesquisa histórica não sobrescreve comportamento
implementado.

## Contratos implementados no headless

- `auth.json` é somente leitura e fail-closed. No Windows, o arquivo é aberto
  por handle `windows-sys` e recebe DACL protegida com allowlist exata do owner
  atual, usuário atual, `SYSTEM` e `Administrators`; há teste de caminho Unicode.
  Symlink, reparse point, arquivo não regular, ACL divergente e schema inválido
  falham; arquivo ausente não é criado.
- Redirects HTTP são desabilitados. O cache opcional é em memória, por processo,
  com namespace seguro por provider/endpoint/modelo e mensagens/tools/conteúdo
  canonicalizados; headers e credenciais não entram na chave, e tool calls nunca
  são cacheadas. A implementação é testada, mas os caminhos normais usam
  `HttpProviderClient::new`, sem cache ativo.
- SSE exige terminação estrita. OpenAI-compatible e Anthropic preservam IDs de
  provider nos tool deltas quando fornecidos; chamadas legadas sem ID recebem
  identificador interno. JSON de tool malformado ou incompleto é rejeitado.
- O loop checa `max_tool_calls` antes de side effects, emite `ToolStarted`,
  mantém prompt raiz e pares assistant/tool na compactação bounded do mesmo
  modelo/adapter, e persiste `Usage` inclusive do resumo.
- A API key fornecida é redigida exatamente antes de tool output, follow-up,
  renderização e sessão. Não há afirmação de detecção automática de segredos
  desconhecidos.
- `--image` é repetível para PNG/JPEG/GIF/WebP; exige arquivo regular
  não-symlink, não vazio e <=20 MiB. `SLIM_CONTEXT_WINDOW_TOKENS` e
  `SLIM_MAX_OUTPUT_TOKENS` são inteiros positivos; o cap de saída padrão é
  4096 tokens e o resultado de tool é limitado a 64 KiB.
- Headless text/JSONL expõe `stop`: `provider_completed`/exit `0`,
  `turn_limit` ou `repeated_failed_tool`/exit `12`, e `tool_limit`/exit `22`.

## Evidência e limites

O gate atual é `cargo test --workspace` + `refresh-slim.ps1 -Test`, ambos
verdes: 220 passed / 0 failed / 1 ConPTY físico ignored em 45 suítes e
0 warnings no build; release implantado e smoke test aprovado. Clippy focado
em `slim-core --all-targets -D warnings` também está verde. O workspace ainda
tem drift de `cargo fmt --check`; Clippy para em 6 diagnósticos preexistentes de
`slim-tui`, e a passada `slim-cli --no-deps` revela mais 21 preexistentes. O E2E
inclui o binário real contra fixtures
localhost. Nenhum provider live foi chamado e não foi executada a matriz
física completa de terminal, IME, mouse ou clipboard.

O release contém somente `slim.exe`; o builder usa caminhos relativos ao próprio
script e metadados ZIP fixos. Verifique-o com `python3 release/build_release.py`
(duas vezes) e `sha256sum -c release/SHA256SUMS.txt`.

As fatias de core/headless e os componentes TUI estão no workspace; a integração
pendente está listada no plano. Limitações físicas continuam separadas das
lacunas de wiring do produto.
