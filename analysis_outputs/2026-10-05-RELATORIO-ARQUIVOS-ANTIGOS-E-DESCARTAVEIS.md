# Arquivos antigos e descartáveis do Slim — inventário de tamanho

Data da medição: **2026-10-05 03:53–03:56 (-03:00)**, workspace
`C:\Users\Thiago Emanuel\Projects\Slim`. Este relatório é somente medição: nada
foi apagado, movido ou comitado.

## 1. Resumo

| Métrica | Valor |
|---|---:|
| Workspace completo (inclui `target/` e `.git/`) | **174.725 arquivos / 102,31 GiB** |
| Descartável e ignorado pelo git (`git clean -ndX`) | **174.021 arquivos / ≈102,30 GiB** |
| Conteúdo versionado real (código, docs, testes) | 613 arquivos rastreados, ≈0,04 GiB |
| `target/` (fração do descartável) | **173.999 arquivos / 102,27 GiB = 99,97%** |
| Sem `target/`, o workspace cai para | ≈0,04 GiB (≈41 MiB) |
| Estado de runtime de sessões (dentro e fora do repo) | 32 arquivos / ≈5,4 MiB |
| Dumps de memória clássicos (`*.dmp`, `*.mdmp`, `*.core`, `heapdump`, `hs_err_pid*`) | **0 arquivos / 0 B** |

Volume `C:` no momento: 350,3 GB usados, 602,5 GB livres — o `target/` é 29,2%
do uso do volume.

## 2. Descartável ignorado pelo git (`git clean -ndX` lista exatamente 6 entradas)

| # | Caminho | Arquivos | Tamanho | Conteúdo |
|---|---|---:|---:|---|
| 1 | `target/` | 173.999 | 102,27 GiB | Artefatos de build Rust (regeneráveis) |
| 2 | `crates/slim-cli/.slim/` | 7 | 2,59 MiB | Dumps `context-history-*` + artefatos de tool |
| 3 | `bench/luna-live/__pycache__/` | 6 | 192,4 KiB | Bytecode Python |
| 4 | `bench/agentic-code-intel/__pycache__/` | 3 | 53,2 KiB | Bytecode Python |
| 5 | `.slim/` (raiz do projeto) | 5 | 0,29 MiB | Sessão TUI **ativa** + fila + 1 shell-log |
| 6 | `.claude/settings.local.json` | 1 | 905 B | Config local da ferramenta |
| | **Total ignorado** | **174.021** | **≈102,30 GiB** | |

### 2.1 Decomposição de `target/` (102,27 GiB)

| Subdiretório | Arquivos | Tamanho |
|---|---:|---:|
| `target/debug/` | 164.187 | 97,83 GiB |
| — `debug/incremental/` | 141.048 | 48,94 GiB (493 dirs com mtime > 7 dias = 10,62 GiB) |
| — `debug/deps/` | 8.749 | 47,89 GiB (0 arquivo > 14 dias: tudo recém-recompilado) |
| — `debug/build/` | 1.160 | 0,45 GiB |
| — `debug/.fingerprint/` | 13.206 | < 0,01 GiB |
| `target/release/` | 6.809 | 3,11 GiB |
| `target/clippy/` | 2.385 | 1,28 GiB (cache de lint; `cargo clean` padrão **não** remove) |
| `target/cargo-timings/` | 417 | 0,05 GiB |
| `target/test-logs/`, `target/tui-snapshots/`, `target/tui-snapshots-before/` | 53 + 69 + 51 | < 0,01 GiB no total |
| `target/tmp/` | 0 | 0 B |

`target/clippy`, `target/cargo-timings`, `target/test-logs` e `target/tui-snapshots*`
não são removidos por `cargo clean` sem argumentos.

### 2.2 Estado de runtime fora do repositório (não rastreado por `git clean`)

| Caminho | Arquivos | Tamanho | Observação |
|---|---:|---:|---|
| `%USERPROFILE%\.slim\` | 20 | 2,47 MiB | 2 sessões TUI antigas (1,87 MiB), artefatos de shell/read, `auth.json` (2.640 B) e caches `*-models.json` |
| `%APPDATA%\slim\config\slim.toml` + `%APPDATA%\Slim\config\slim.toml` | 2 | 110 B | Configuração duplicada |
| `%LOCALAPPDATA%\CrashDumps` | — | — | Diretório inexistente |

**`auth.json` guarda credenciais.** É o único item deste levantamento que não deve
ser apagado por rotina de limpeza; se for removido, é perda de autenticação, não
recuperação de espaço (2,6 KiB).

## 3. "Despejos de memória"

Não existe nenhum dump de memória clássico no workspace: 0 arquivos para
`*.dmp`, `*.mdmp`, `*.core`, `*.heap`, `*.hprof`, `hs_err_pid*.log`, `heapdump*`
e nenhum diretório `CrashDumps`. O que o Slim grava como memória de agente é
transcrição JSONL e snapshot de contexto:

| Tipo | Arquivos | Tamanho |
|---|---:|---:|
| Transcrições de sessão (`sessions/tui-*.jsonl`) | 3 | 2,15 MiB |
| Snapshots de contexto (`context-history-*` em `crates/slim-cli/.slim/artifacts/`) | 4 | 2,59 MiB |
| **Total equivalente a despejo de memória** | **7** | **≈4,74 MiB** |

Detalhe dos maiores: `context-history-665d0dcf…` 2.000.482 B,
`%USERPROFILE%\.slim\sessions\tui-1790034083999000500-7040-1.jsonl` 1.205.231 B,
`tui-1790969985957592200-76524-1.jsonl` 755.909 B,
`.slim\sessions\tui-1791183183819145000-47708-1.jsonl` 292.810 B (sessão **em
execução** — tem `.lock` irmão e mtime igual ao da medição).

Esses volumes são irrelevantes em GB (4,74 MiB); o problema de espaço é
exclusivamente `target/`.

## 4. Arquivos "antigos" (históricos versionados)

A idade por mtime não ajuda: o clone tem menos de 30 dias e não há arquivo entre
30 e 365 dias. Distribuição: `<7d` = 115.033 arquivos / 65,89 GiB;
`7–30d` = 59.597 / 36,41 GiB; `>365d` = 95 arquivos / 0,01 GiB (todos dentro de
`target/`, fontes de registry com timestamp preservado).

O que existe de fato é documentação histórica datada (2026-08-30 a 2026-09-22):

| Conjunto | Arquivos | Tamanho | Situação |
|---|---:|---:|---|
| `analysis_outputs/` (auditorias e benchmarks datados) | 55 | 0,76 MiB | Evidência — preservar (RULES.md §R5, `analysis_outputs/README.md`) |
| `release/history/` (`2026-09.md` 157.673 B, `2026-10.md`, `README.md`) | 3 | 0,15 MiB | Diário de releases — preservar |
| `docs/history/` (`README-2026-09-20.md` 55.567 B, `PROJECT-INDEX-2026-09-06.md`) | 2 | 0,07 MiB | Arquivado — preservar |
| `poc/` (search, toolchain, tui) | 15 | 0,08 MiB | Legado; `LIMPEZA-2026-09-16.md` §6 registra o POC como já quebrado — candidato a arquivar, não a apagar por espaço |
| `.git/` | 91 | 25,12 MiB | 3 packs (10,32 MiB), 495,87 KiB soltos, **0 lixo** |

Nenhum desses conjuntos é grande o suficiente para justificar remoção por espaço:
os três primeiros somam **1,0 MiB em 60 arquivos**.

## 5. Limpeza executada (2026-10-05 03:58–03:59)

`cargo clean` (raiz do workspace) — saída literal: `Removed 174101 files, 102.4GiB total`, exit 0.
O diretório `target/` foi removido por inteiro, incluindo `clippy`, `cargo-timings`,
`test-logs` e `tui-snapshots*`. Volume `C:`: 351,30 → 252,40 GB usados
(98,9 GB liberados medidos por `Get-PSDrive`). Workspace após a limpeza:
**729 arquivos / 43.099.638 bytes (≈41,1 MiB)**, sem `target/`.

Não foi apagado nada fora de `target/`: `bench/*/__pycache__` (≈240 KiB),
`.slim/`, `crates/slim-cli/.slim/`, `%USERPROFILE%\.slim\` (inclui `auth.json`),
`analysis_outputs/`, `docs/history/`, `release/history/` e `poc/` permanecem.
Nenhum build foi executado após a limpeza; a validação do próximo build fica
pendente. Durante a medição pós-limpeza, `git status` mostrava 11 arquivos `M` e 2
`??` novos em `crates/slim-tui/` e `docs/DESIGN-SLIM-TUI.md` com mtime entre
03:57 e 04:07, ou seja, **WIP concorrente de outra sessão**, não produzido aqui;
nada disso foi modificado, revertido ou comitado por esta limpeza. `Cargo.lock`
permaneceu intacto (67.077 bytes, 29/09/2026 20:24:36).

## 6. Recomendação de limpeza (inventário original, para referência)

Ganho real está em um único alvo:

| Ação | Libera | Risco |
|---|---:|---|
| `cargo clean` (remove `target/` inteiro) | 102,27 GiB / 173.999 arquivos | Baixo: recompilação completa depois |
| Remover só `target/clippy`, `target/cargo-timings`, `target/test-logs`, `target/tui-snapshots*` | ≈1,33 GiB | Nenhum: caches de lint e logs antigos |
| Apagar `bench/*/__pycache__` | 240 KiB | Nenhum |
| Apagar as 2 sessões antigas de `%USERPROFILE%\.slim\sessions\` + artefatos | ≈2,4 MiB | Baixo (histórico de 21/09 e 02/10) |
| Apagar `.slim/` ou `crates/slim-cli/.slim/` | 0,29 MiB + 2,59 MiB | **Não apagar agora**: contém a sessão ativa desta execução |

O que **não** vale apagar por espaço: `analysis_outputs/`, `docs/history/`,
`release/history/`, `poc/` (política do repositório preserva evidência histórica)
e `%USERPROFILE%\.slim\auth.json` (credenciais).

## 7. Método e reprodução

Medições por `Get-ChildItem -Force -Recurse -File | Measure-Object Length -Sum`
em PowerShell 7 no volume local; classificação de descartável por
`git clean -ndX` e `git status --porcelain --ignored=matching`; dumps por glob de
extensão em todo o workspace. Tamanhos em GiB = bytes/1024³. Como `target/` muda a
cada build, os números valem para o instante da medição; `.slim/sessions/*.jsonl`
cresce enquanto a sessão está aberta.
