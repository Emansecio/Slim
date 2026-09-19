# Bootstrap numa maquina limpa (Windows)

## 1. Pre-requisito (o script NAO instala)

- Rust **stable MSVC** (rustup) com `cargo` no PATH.
- Nao existe `rust-toolchain.toml`, entao vale o toolchain instalado na maquina.

## 2. Automatico

```powershell
git clone https://github.com/Emansecio/Slim.git
cd Slim
.\bootstrap.ps1            # confere o toolchain + cargo build --release -p slim-cli
.\bootstrap.ps1 -Deploy    # idem + copia para %USERPROFILE%\bin\Slim.exe
.\bootstrap.ps1 -Test      # roda cargo test --workspace antes do build
```

## 3. Publicar o comando `slim` no PATH

O binario em `target\release\slim.exe` **nao** e o que o terminal executa. O
caminho suportado e `%USERPROFILE%\bin\Slim.exe`:

```powershell
cargo build --release -p slim-cli
Copy-Item target\release\slim.exe "$env:USERPROFILE\bin\Slim.exe" -Force
```

O `refresh-slim.ps1` do repo faz build + copia + smoke test, mas **tem caminhos
fixos da maquina de origem** (`C:\Users\User\scoop\persist\rustup-msvc\...\rustc.exe`,
`C:\Users\User\bin\Slim.exe`): em outro PC/usuario, ajuste essas linhas (o unico
fallback e usar o `cargo` do PATH).

## 4. Configuracao (opcional)

| Camada | Caminho |
|---|---|
| Projeto | `./slim.toml` (diretorio de trabalho) |
| Global | `%APPDATA%\slim\slim.toml` |

Chaves reconhecidas: `model`, `endpoint`, `effort`, `max_turns`,
`max_mutating_tool_calls`, `max_read_tool_calls`, `max_output_tokens`,
`timeout_secs`, `max_result_bytes` e `[compaction]`. Arquivo ausente e normal;
arquivo presente e invalido **aborta** com erro nomeando o caminho. Env vence o
TOML: `SLIM_MODEL`, `SLIM_ENDPOINT`, `SLIM_EFFORT`, `SLIM_MAX_TURNS`,
`SLIM_TIMEOUT_SECS`, `SLIM_COMPACTOR`.

## 5. Autenticacao

Abra a TUI e use `/login` (OAuth PKCE: Anthropic Claude Pro/Max ou OpenAI Codex
ChatGPT Plus/Pro). A credencial fica em `%USERPROFILE%\.slim\auth.json`.
Alternativa por chave: `SLIM_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` ou
`SLIM_AUTH_FILE`. Precedencia: env > `oauth` do auth.json > `api_key` do auth.json.
**Sem credencial o binario abre e roda offline** (provider fake dos testes).

## 6. Peculiaridades do repo

- `.cargo/config.toml` fixa `jobs = 1` por limite de memoria da maquina de origem:
  numa maquina melhor, remova ou relaxe (o build fica bem mais rapido).
- Nao ha tag, release nem CI: o binario so existe depois de buildar aqui.
- `release/build_release.py` empacota `target/release/slim.exe` num zip
  deterministico (`release/slim-0.1.0-windows-x64.zip`) + `manifest.json` +
  `SHA256SUMS.txt`, caso queira distribuir depois.
- Nao vem no clone: `target/` e `%USERPROFILE%\.slim\` (auth e sessoes).