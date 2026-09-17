# gauge-prepare-cost

Mede o custo por turno do pipeline real de request do provider em `slim-core`,
contra um servidor SSE loopback (127.0.0.1). Sem rede, sem conta de provider,
sem build release — debug only.

Autorizado por Devin em 2026-09-16. Criado porque nenhum harness existente
(agentic-code-intel, economy-next, luna-live, token-economy) mede o custo de
CPU/alocacao de preparacao de request por turno.

## O que mede

Todas as arms executam codigo de producao de `slim-core` (nao ha stubs no
caminho medido; o adapter e o `OpenAiCodexAdapter` real):

| arm | caminho | isola |
|-----|---------|-------|
| `loop` | `run_agent_loop_with_messages` + codex real | turno completo do agent loop |
| `loop-noenv` | mesmo loop, adapter newtype com `request_envelope_upper_bound_chars() == None` | forca prepare eager no preflight; NAO isola P1 (o `?` do envelope fica no fim do estimate — ambos os arms pagam o fold) |
| `direct` | `run_provider_messages_turn` com N sensitive values registrados | clone+replace de `redact_messages` (P2) |
| `direct-nosens` | mesmo direct, sem sensitive values | baseline prepare+send |
| `micro` | ops serde/chars sobre o body real capturado | custo por passe (P3 e decomposicao do prepare) |

O servidor responde um SSE fixo (`response.output_text.delta` +
`response.completed` com usage) e captura o body de cada request — usado nos
micro-benches e reportado como `body_bytes`.

Historico sintetico: triplas user(2KiB)/assistant+tool_call/tool_result(~12KiB,
abaixo de `max_result_bytes` 16KiB), com segredos embutidos a cada 8 turnos
para exercitar `String::replace`.

## Uso

```bash
cargo build            # workspace isolado: nao toca D:\Slim\target nem o lock
./target/debug/gauge-prepare-cost.exe --reps 7 --sizes 64,256,1024
```

`--sizes` em KiB de conteudo de historico alvo. Saida CSV em stdout.

## Interpretacao

- `loop - loop-noenv` ~= 0 NAO isola o estimate: em
  `estimate_unprepared_request_chars` (runtime/mod.rs:7136) o
  `request_envelope_upper_bound_chars()?` so e avaliado no FIM (7211), depois
  dos folds sobre todo o historico — ambos os arms pagam a varredura; o arm B
  apenas descarta o resultado e faz o prepare eager no preflight (reusado em
  1954). O custo real do estimate e medido pelos proxies
  `estimate_bytes_fold_current` vs `estimate_chars_fold_prefix` (~5 vs ~7 ms
  @1 MB — o fixer trocou chars() por bytes() em mod.rs:7224).
- `direct - direct-nosens` = redacao com segredos (runtime/mod.rs:1206-1211 ->
  5574-5582). O short-circuit sem-segredos em 1207 e um fix nao-commitado;
  pre-fix o piso por request era `history.to_vec()` (medido: ~0,15 ms @1 MB)
  + to_owned por campo.
- `loop - direct-nosens` = overhead de entrada do run: estimate (~5 ms @1 MB)
  + to_vec (1539) + `conversation.clone_from` (1554) + elisao (1553) +
  overlay + scans (messages_are_text_only 2071). `to_vec` medido = barato.
- `micro`: `json_encode_body` e `components_items_writer` sao dois passes de
  ~mesmo custo (~10 ms @1 MB cada) — evidencia de que o accounting do
  PreparedProviderRequest dobra a serializacao por turno.

Secao extra (rodada 2): fabrica uma sessao duravel real via `JsonlRepo` e mede
`preflight_session` + `provider_messages_from_records` + `open_no_repair` — o
trabalho que `--resume` faz antes de chamar o provider (headless.rs:432+).

## Limites

- Profile dev com `debug-assertions = false` (evita abort no assert de
  preflight runtime/mod.rs:1965 para shapes sinteticos; nao altera trabalho
  medido — o assert e check, nao trabalho).
- Numeros absolutos sao debug-build; comparacoes entre arms sao validas
  porque todas pagam o mesmo profile.
- SSE fixo = turno de 1 provider call, sem tool execution real.
