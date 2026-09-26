# Slim

**Visual das mensagens, 20/09/2026:** cabeçalho `● Você` em linha própria,
corpo alinhado ao rótulo e fundo igual ao histórico. Apenas o marcador verde
recebe realce breve ao enviar; com movimento reduzido, permanece estático.

Harness de coding agent em Rust para Windows x64/MSVC. O projeto reúne CLI
headless, TUI fullscreen, providers, ferramentas nativas, sessões, compactação e
integração LSP em um único workspace.

> **Estado:** checkpoint de integração parcial (`0.1.0`), ainda não uma v1
> concluída. O estado executável é definido pelo código; relatórios e medições
> datados são históricos.

## Começo rápido

Requer Rust stable com toolchain MSVC e `cargo` no `PATH`.

```powershell
git clone https://github.com/Emansecio/Slim.git
cd Slim
.\bootstrap.ps1
.\bootstrap.ps1 -Test -Deploy
```

Sem configuração de provider, o Slim pode abrir no fluxo offline/fake. Para os
fluxos autenticados, abra a TUI e use `/login`, ou configure a variável de
ambiente documentada para o provider escolhido.

```powershell
slim
slim --headless --fake --prompt "hello"
```

Veja [BOOTSTRAP.md](BOOTSTRAP.md) para instalação em uma máquina limpa e a
[referência detalhada de CLI/runtime](docs/reference/CLI-AND-RUNTIME.md) para
configuração, providers, MCP, autenticação e contratos operacionais.

## Build, testes e instalação local

```powershell
cargo build --workspace
cargo test --workspace
.\refresh-slim.ps1          # release + cópia para o PATH + smoke
.\refresh-slim.ps1 -Test    # inclui cargo test --workspace
.\refresh-slim.ps1 -FastBuild # ciclo local com menos otimização; veja release/README.md
```

O comando `slim` instalado é uma cópia estática em
`%USERPROFILE%\bin\Slim.exe`; ele não acompanha o checkout automaticamente.
Alterações no código do Slim devem terminar pelo fluxo de publicação descrito em
[AGENTS.md](AGENTS.md) e [RULES.md](RULES.md).

## Configuração

O Slim combina configuração nesta precedência:

**CLI > ambiente > `./slim.toml` > configuração global > padrão interno**

| Camada | Caminho |
|---|---|
| Projeto | `./slim.toml` |
| Global | `%APPDATA%\slim\config\slim.toml` |

Exemplo mínimo:

```toml
model = "gpt-4o-mini"
max_turns = 128
timeout_secs = 120

[compaction]
enabled = true
background = true
```

Arquivos presentes e inválidos causam erro explícito. Credenciais não devem ser
armazenadas em `slim.toml`.

A recuperação do provider limita falhas consecutivas e o total por execução,
respeita `Retry-After` e mantém a causa da parada na conversa. Consulte a
[política de recuperação](docs/DESIGN-SLIM-TUI.md#243-falha-de-effect).

## Estrutura do repositório

| Caminho | Responsabilidade |
|---|---|
| `crates/slim-core/` | runtime, providers, ferramentas, contexto e persistência |
| `crates/slim-cli/` | binário, headless, configuração e composição do produto |
| `crates/slim-tui/` | estado, reducer, renderização e interação terminal |
| `crates/slim-lsp/` | integração semântica via Language Server Protocol |
| `tests/` | testes de integração entre crates registrados pelo `slim-cli` |
| `docs/` | contratos, arquitetura, planos e índice documental |
| `bench/` | harnesses e relatórios de benchmark |
| `poc/` | provas de conceito históricas fora do workspace principal |
| `analysis_outputs/` | auditorias e evidências datadas |
| `release/` | empacotamento, deploy vigente e histórico de releases |

## Documentação

- [Índice canônico](docs/README.md)
- [Contrato visual e funcional da TUI](docs/DESIGN-SLIM-TUI.md)
- [Arquitetura](docs/RUST-CLI.md)
- [Plano de implementação](docs/PLANO-IMPLEMENTACAO.md)
- [Próximas etapas](docs/PROXIMAS-ETAPAS-AGENTE.md)
- [Referência detalhada de CLI e runtime](docs/reference/CLI-AND-RUNTIME.md)
- [Histórico de mudanças](CHANGELOG.md)
- [Auditorias e evidências](analysis_outputs/README.md)
- [Release e deploy vigente](release/README.md)

## Contribuição por agentes

Leia [AGENTS.md](AGENTS.md) e as seções pertinentes de [RULES.md](RULES.md)
antes de alterar o checkout. Preserve trabalho não relacionado e não use
relatórios históricos como fonte do estado atual sem revalidar o código.

## Licença

Licenciado sob Apache License 2.0. Consulte [LICENSE](LICENSE). Dependências e
materiais de terceiros mantêm suas próprias licenças.
