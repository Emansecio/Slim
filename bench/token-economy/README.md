# Benchmarks de economia de tokens — Slim x Pi x Pit

Benchmark **real via Codex**, uma tarefa curta com GPT‑5.6 Luna em 05/09/2026:
[Pi × Slim: resultado, auditoria por chamada e oportunidades de correção](../luna-live/README.md).
O método de captura simulada abaixo é uma campanha diferente.

Harness que mede o payload **real no fio** de três coding agents executando a
**mesma tarefa** contra um endpoint localhost de captura:

| Agente | Binário | Transporte |
|---|---|---|
| Slim | `C:\Users\User\bin\Slim.exe` | OpenAI-compatível (`--provider openai`) e Anthropic |
| Pi | `pi` (`@earendil-works/pi-coding-agent`, Node) | `openai-completions` via `models.json` |
| Pit | `pit` (fork em `C:\PiTest`) | `openai-completions` via `models.json` |

## Método

1. `capture_server.py` sobe em `127.0.0.1:8931`. No **primeiro** request após
   `POST /reset`, ele inspeciona o array `tools` do próprio agente, descobre a
   tool de leitura (nome + propriedade de caminho/limite) e responde com uma
   chamada real dessa tool para `bench-target.txt`. Nos requests seguintes,
   responde texto final. Assim todos os agentes executam o mesmo trabalho
   (1 leitura + resposta) usando as próprias ferramentas — nenhum fixture
   conhece o formato específico dos agentes.
2. Cada request capturado vira JSON em `captures/`; `analyze.py` mede:
   bytes totais, bytes do system prompt, bytes do bloco de tools (+ contagem),
   bytes do histórico, maior tool result e ~tokens (÷4, estimativa).
3. Tarefa idêntica: cwd temporário com `bench-target.txt` (conteúdo fixo);
   prompt: "Read bench-target.txt and tell me its first line."
4. Pi/Pit rodam com `USERPROFILE` redirecionado para home isolado
   (`bench/token-economy/.homes/<agente>`), então o benchmark não toca na
   configuração real do usuário; o provider é declarado em
   `models.json` dentro desse home isolado.

## Execução

```powershell
python bench/token-economy/capture_server.py   # terminal 1
powershell -File bench/token-economy/run_benchmark.ps1
```

Resultados consolidados: `RESULTS.md`.
