# POCs da TUI

> **Status de implementação:** evidência de componentes/lifecycle da TUI. O
> comando `Slim` já abre a TUI e usa composer, provider/loop/tools reais, streaming,
> usage e cancelamento; matriz física completa continua fora desta evidência.

Área para os POCs de lifecycle fullscreen, contrato visual, composer/input,
performance/virtualização e capabilities.

Os gates normativos estão em
`Documentações - Projeto/VALIDACAO-VIABILIDADE-RATATUI-GROK-BUILD.md`.

Harness executável: `cargo run --manifest-path poc/tui/Cargo.toml -- --cycles 100`.
O comando precisa de um terminal real/ConPTY; execução sem TTY registra falha de
capability, não valida fullscreen.

Corpus terminal: `cargo run --release --manifest-path poc/tui/Cargo.toml -- --long-session-probe`.
