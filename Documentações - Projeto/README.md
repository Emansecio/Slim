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
> com gate (p95 W8 2,384 ms ≤ 16 ms). O harness v2 concluiu resume/observabilidade
> e a capability bridge durável para Skill/MCP/child/Todo/Plan/Goal. A TUI também
> integra inspectors responsivos, busca no transcript, cópia nativa, anexos locais,
> composer adaptativo e código/diff semântico. Pendente: gate físico Windows
> (PTY/console real), projeção completa de Plan/Goal e métricas exportadas.

Este índice aponta o contrato, o plano e a evidência da versão `0.1.0`; não
declara que todos os contratos já estejam integrados.

## Matriz de implementação atual

| Área | Estado |
|---|---|
| Headless | Integrado: providers OpenAI-compatible, Anthropic, Codex subscription e OpenCode Go (Chat Completions/Responses/Messages), tools nativas, modos, auth DACL, imagem local, compaction, usage, artifacts e anti-loop. |
| TUI | Integrada ao mesmo provider/loop/tools: `/login` inclui OpenCode Go com chave mascarada; `/models` usa catálogo live/cache/fallback, grupos e viewport; `ask_question` oferece opções e `Outro...` em runs comuns Auto/ReadOnly; composer adaptativo com cursor real, stream incremental em lanes bounded, tool blocks tipados agregados, Markdown/código/diff semânticos, inspectors, busca, cópia, anexos locais, usage, modos, cancelamento, scroll pin/live-edge, command palette e restauração fullscreen; golden matrix via TestBackend; PTY E2E físico pendente (`#[ignore]`, requer console real); sem credencial/provider pago nos gates. |
| Cache/HTTP | Replay local de respostas (`ProviderCache`) desligado no produto; transporte Reqwest compartilhado reaproveita conexões sem compartilhar adapter ou autenticação. |
| Sessões | Writer/recovery/branch e `--session` existem; `--resume`/recovery explícitos existem headless e TUI; UX geral de seleção/fork continua limitada. |
| Skills/MCP | Tool `skill` lazy; `%USERPROFILE%/.slim/skills` (cwd `.slim/skills` vence). Sem catálogo no schema. Sem MCP. |
| Subagentes | Scheduler/child-session duráveis têm fila bounded, cancelamento e reopen; não há child agent provider-backed ou processo externo real. |
| Todo/Plan/Goal | Tool `todo` + dock via `TodoChanged`. Plan/Goal sem UI; Plan headless aborta (N4). |
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
8. [HARNESS-V2-TRACKER.md](HARNESS-V2-TRACKER.md) — tracker canônico do Slim
   Durable Harness v2, suas 10 etapas, fronteiras da Onda 1 e evidência.
9. [PROXIMAS-ETAPAS-AGENTE.md](PROXIMAS-ETAPAS-AGENTE.md) — fila ranqueada do
   ciclo “agente completo, harness leve” (N1–N7 ACEITOS, WONTFIX, três
   sessões imediatas). Prevalece sobre o PLANO §10.1 para o que fazer *agora*.

Quando houver conflito, prevalecem as decisões finais, depois este plano e os
contratos específicos da TUI. Pesquisa histórica não sobrescreve comportamento
implementado. Para o próximo wiring do agent loop (não M3 visual), use o
item 9.

## Contratos implementados no headless

- OpenCode Go usa provider lógico próprio e registro fechado de 24 modelos. O
  modelo define wire (Chat Completions, Responses ou Messages), contexto,
  output, reasoning e imagem. Precedência da chave: `SLIM_API_KEY` >
  `OPENCODE_API_KEY` > auth file. A TUI salva/remove somente essa entrada;
  no próximo startup, restaura automaticamente o provider API-key ativo.
  OAuth permanece tipado, com refresh preservado; auth file inválido é erro
  explícito, sem simular estado deslogado e sem superar uma credencial de env;
  `/models` consulta o catálogo público com timeout/redirect/bounds, persiste
  cache atômico e mantém fallback offline sem aceitar IDs desconhecidos.
  No wire Chat Completions, snapshots cumulativos de usage são consolidados
  antes de chegar ao runtime; os demais providers mantêm validação estrita.
- `auth.json` é somente leitura e fail-closed. No Windows, o arquivo é aberto
  por handle `windows-sys` e recebe DACL protegida com allowlist exata do owner
  atual, usuário atual, `SYSTEM` e `Administrators`; há teste de caminho Unicode.
  Symlink, reparse point, arquivo não regular, ACL divergente e schema inválido
  falham; headless não cria arquivo ausente. Login OpenCode Go explícito na TUI
  pode criá-lo atomicamente, preservando providers irmãos.
- Redirects HTTP são desabilitados. O cache bounded é em memória, por processo,
  com namespace seguro por provider/endpoint/modelo e mensagens/tools/conteúdo
  canonicalizados; headers e credenciais não entram na chave, e tool calls nunca
  são cacheadas. Os caminhos normais usam cache e transporte compartilhado;
  configuração semântica e escopo de credencial continuam na chave.
- SSE exige terminação estrita. OpenAI-compatible e Anthropic preservam IDs de
  provider nos tool deltas quando fornecidos; chamadas legadas sem ID recebem
  identificador interno. Em Chat Completions, material de tool é provisório até
  o terminal: `stop` normal o descarta, enquanto terminal de ferramenta valida
  o lote integral e rejeita qualquer chamada malformada ou incompleta.
- O loop aplica budgets separados read-only vs mutating por run; esgotamento → `tool_limit` (exit 22). Teto de turns por run: 128 (`SLIM_MAX_TURNS` / `max_turns` em `slim.toml`, cap 1024); esgotamento → `turn_limit` (exit 12) com `Turn limit reached (N/N)`. `search` bounded (gitignore, skip de build dirs, cap/paginação).
- A API key fornecida é redigida exatamente antes de tool output, follow-up,
  renderização e sessão. Não há afirmação de detecção automática de segredos
  desconhecidos.
- `--image` é repetível para PNG/JPEG/GIF/WebP; exige arquivo regular
  não-symlink, não vazio e <=20 MiB. `SLIM_CONTEXT_WINDOW_TOKENS` e
  `SLIM_MAX_OUTPUT_TOKENS` são inteiros positivos; o cap/reserva padrão é
  4096 tokens. No Codex subscription ele permanece reserva local e não envia
  `max_output_tokens`; providers que suportam o campo continuam recebendo-o.
  O resultado de tool é limitado a 64 KiB.
- Headless text/JSONL expõe `stop`: `provider_completed`/exit `0`,
  `turn_limit` ou `repeated_failed_tool`/exit `12`, e `tool_limit`/exit `22`.
- `--verbose` no headless text acrescenta timeline humana de tools e o Usage
  Ledger v2; JSONL v2 expõe requests, cache, compactação, custos e validação. O
  padrão continua answer-first; `--verbose --jsonl` é inválido.

## Evidência e limites

O gate atual é `cargo test --workspace`, verde: 1087 passed /
0 failed / 1 ignored (ConPTY físico) em 87 suítes e 0 compiler warnings (1 teste
preexistente quebrado filtrado via `--skip`: `tui_bridge
ordinary_tui_second_turn_sends_prior_user_and_assistant`, sem implementação em
`crates/`); release implantado e smoke
test aprovado por via manual equivalente (`refresh-slim.ps1` aborta sob
`ErrorActionPreference=Stop` do harness). `cargo check --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings` e
`git diff --check` também estão verdes (`cargo fmt --all -- --check` acusa somente
drift preexistente do rustfmt 1.98). A fixture localhost exercita o turno
completo pela API TUI, sem provider live. O smoke ConPTY do binário implantado
foi executado, mas este host emitiu só o probe `ESC[6n`, sem frame; a matriz
física completa de terminal, IME, mouse e clipboard permanece não observada.

No deploy atual, `target\release\slim.exe` e `C:\Users\User\bin\Slim.exe` têm
18.706.944 bytes e SHA-256 idêntico
`3586F8F4B080C0E540073B404742254F3D56513C51D0EA90281C9864A3D62A76`;
`slim --version` retorna `slim 0.1.0` com exit `0`.

O release contém somente `slim.exe`; o builder usa caminhos relativos ao próprio
script e metadados ZIP fixos. Verifique-o com `python3 release/build_release.py`
(duas vezes) e `sha256sum -c release/SHA256SUMS.txt`.

As fatias de core/headless e os componentes TUI estão no workspace; a fila
curta de wiring do agente (OAuth headless, Todo/Skill no loop, Plan TUI) está
em [PROXIMAS-ETAPAS-AGENTE.md](PROXIMAS-ETAPAS-AGENTE.md). A integração v1
ampla (MCP transporte, child real, E2E) permanece no plano. Limitações
físicas continuam separadas das lacunas de wiring do produto.
