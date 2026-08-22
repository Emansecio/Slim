# AGENTS.md — regras obrigatórias para agentes de código

> **Leia primeiro: [RULES.md](RULES.md) — protocolo canônico anti-alucinação.**
> Prevalência: `RULES.md` > este arquivo > README > memória da conversa.
> Este arquivo resume o que mais mata entregas aqui; RULES.md detalha tudo com
> os incidentes reais que motivaram cada regra.

---

## 🚨 REGRA ZERO: atualizar o binário implantado antes de encerrar

O comando `slim` do terminal aponta para `C:\Users\User\bin\Slim.exe`, uma
**cópia estática** do `target\release\slim.exe`. Ela **não se atualiza
sozinha** — código novo sem deploy é, para o usuário, como se não existisse.

**Depois de QUALQUER mudança de código (Rust, testes, Cargo.toml), rode:**

```powershell
.\refresh-slim.ps1          # build release + copia para o PATH + smoke test
```

Só declare a tarefa concluída depois que o script imprimir `OK:`. Se a mudança
for estrutural (não cosmética), prefira `.\refresh-slim.ps1 -Test`, que roda
`cargo test --workspace` antes do deploy e aborta em caso de falha.

> **Incidente de referência (2026-08-21, I1 em RULES.md):** o usuário validou
> visualmente a TUI rodando um `Slim.exe` das 05:45 enquanto o código já tinha
> os slices do dia. Resultado: screenshots da interface antiga e um ciclo
> inteiro de "documentação vs realidade" desperdiçado. Não repita isso.

## 🔧 Toolchain — armadilhas conhecidas

- As variáveis de usuário `RUSTC`/`CARGO` apontam para
  `C:\Users\User\.cargo\bin` (**não existe**). O `refresh-slim.ps1` define
  `RUSTC` corretamente na sessão; em comandos manuais, use:

  ```powershell
  $env:RUSTC = 'C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
  ```

- Existem duas instalações rustup: `C:\Users\User\.rustup` (rustc **1.95.0**,
  velha) e o scoop persist (rustc **1.97.1**, ativa). Compilar com uma e
  documentar com outra gera E0514 nos Doc-tests. Após trocar de compilador,
  limpe `target\debug` antes de retestar.
- Exit code de pipeline PowerShell pode mentir (`2>&1 | Select-String`):
  confirme com `EXIT=$LASTEXITCODE` sem filtro antes de diagnosticar falha.

## ✅ Definição de pronto

Uma tarefa só está PRONTA quando **todos** os itens valem:

1. `cargo test --workspace` está verde (0 failed, 0 warnings);
2. `.\refresh-slim.ps1` rodou e imprimiu `OK:` (binário no PATH atualizado);
3. Documentação de status atualizada na mesma tarefa:
   - `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` §7 (log de execução);
   - `Documentações - Projeto/DESIGN-SLIM-TUI.md` §1.1 (checkpoint), se o
     comportamento mudou;
   - números de testes conferidos com grep em **todos** os arquivos que os
     citam (`README.md`, `Documentações - Projeto/README.md`,
     `PLANO-IMPLEMENTACAO.md`, `release/README.md`, tracker);
4. Checklist da seção 4 de `RULES.md` copiado preenchido na resposta final.

## 📚 Documentação viva

- `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` é tracker vivo: todo
  slice executado vira linha no §7; bugs novos entram no §5 antes da correção.
- `Documentações - Projeto/DESIGN-SLIM-TUI.md` §§2–29 são contrato normativo:
  layout, motion, degradação e cores são decisões fechadas. Mudar
  comportamento sem revisar a spec primeiro é violação — "modernizar" contra
  a spec não é melhoria.
- Auditorias e análises antigas (`analysis_outputs/`, conversas anteriores)
  descrevem o passado: revalide contra o código antes de usar como base.
