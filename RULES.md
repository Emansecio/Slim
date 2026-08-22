# RULES.md — Protocolo anti-alucinação e anti-esquecimento

> **Arquivo canônico de conduta para agentes de código neste workspace.**
> Prevalência: `RULES.md` > `AGENTS.md` > README > memória da conversa.
> Toda regra abaixo existe por causa de um **incidente real** (seção 3, com
> data). Se você sentiu vontade de pular este arquivo, releia a seção 3.

---

## 1. Verdade absoluta — NUNCA alucinar

### R1 — Nada é "feito" sem evidência da sessão
Proibido afirmar "implementei", "testei", "funciona" sem ter, **nesta sessão**,
o comando executado e a saída correspondente. "Deveria funcionar" não é estado
de entrega; é palpite.

### R2 — Números vêm de execução, nunca de memória ou de docs antigos
Contagens de testes, latências, versões e tamanhos: **rode e conte agora**.
Contagem canônica:

```powershell
cargo test --workspace
# somar "test result: ok. N passed" sobre todas as suítes (inclui Doc-tests)
```

Proibido reutilizar número citado em conversa anterior, README ou tracker sem
revalidar. (Incidente I3: docs diziam "zero referências aos números antigos"
e ainda havia `159 testes / 40 seções` em `release/README.md`.)

### R3 — Caminho:linha só existe se você leu
Proibido citar `arquivo.rs:NN` como evidência sem ter lido o arquivo **nesta
sessão**. Antes de afirmar que algo "existe", "é chamado" ou "está integrado":
busque o símbolo no código. Presença de arquivo ≠ presença de wiring.

### R4 — "Não sei" é resposta; inferência disfarçada de fato é mentira
Incerteza se declara explicitamente ("não verifiquei", "inferência", "não
rodei em console físico"). Proibido preencher lacuna com afirmação plausível.
Rotule toda afirmação: **verificado** / **inferido** / **hipótese**.

### R5 — Docs antigos descrevem o passado, não o presente
`analysis_outputs/`, auditorias e conversas anteriores podem estar
**desatualizados em relação ao código**. Ao confrontar doc vs código, o código
ganha — mas confirme lendo o código, não confiando em nenhum dos dois.
(Incidente I4: auditoria antiga dizia "reducer de 4 ações"; o código já tinha
8. Um agente que confiasse nela teria "corrigido" o que não precisava.)

### R6 — Screenshot e binário velho não são estado atual
Interface observada em execução reflete o binário que rodou, não o código no
disco. Antes de julgar visual/comportamento: confira `LastWriteTime` do
executável contra o último build. (Incidente I1: usuário validou a TUI num
`Slim.exe` das 05:45 enquanto o código já tinha os slices do dia.)

---

## 2. Memória do ambiente — NUNCA esquecer o básico

### R7 — REGRA ZERO: deploy do binário
O comando `slim` aponta para `C:\Users\User\bin\Slim.exe`, **cópia estática**
que não acompanha o código. Após QUALQUER mudança de código:

```powershell
.\refresh-slim.ps1          # obrigatório; -Test para mudanças estruturais
```

Tarefa sem o `OK:` do script na resposta = tarefa não entregue.

### R8 — Toolchain duplo e variáveis quebradas
- `RUSTC`/`CARGO` de usuário apontam para `C:\Users\User\.cargo\bin`
  (**inexistente**). O script define `RUSTC` corretamente; em comando manual,
  aponte para `C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\...`.
- Duas instalações rustup: `C:\Users\User\.rustup` = rustc **1.95.0** (velha);
  scoop persist = **1.97.1** (ativa). Compilar com uma e documentar com a
  outra gera **E0514** nos Doc-tests. Trocou de compilador? Limpe
  `target\debug` antes de retestar. (Incidente I2.)

### R9 — Exit code de pipeline PowerShell mente
`cargo ... 2>&1 | Select-String ...` pode reportar exit 1 com tudo verde.
Confirme com `EXIT=$LASTEXITCODE` **sem filtro** antes de diagnosticar falha.
(Incidente I5.)

### R10 — Teste novo pode ter bug no próprio teste
Teste que falha pode estar errado, não o código. Leia o assert antes de
"consertar" o código. (Incidente I6: `WORD.len()` vs `WORD[0].len()` no
wordmark — o teste pegou o bug do próprio código novo.)

### R11 — Documentação viva é parte da entrega
Mudou comportamento? Atualize, na mesma tarefa:
- `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` §7 (log) — todo slice
  vira linha;
- `Documentações - Projeto/DESIGN-SLIM-TUI.md` §1.1 (checkpoint);
- números de testes em **todos** os arquivos que os citam: `README.md`,
  `Documentações - Projeto/README.md`, `PLANO-IMPLEMENTACAO.md`,
  `release/README.md`, `AUDIT-SLIM-TUI-TRACKER.md`.
Grep pelos números antigos antes de encerrar.

### R12 — Não "conserte" o que a spec normativa define
`DESIGN-SLIM-TUI.md` §§2–29 é contrato. Layout, motion (ex.: ambient idle ≤
2 fps, um glyph só), degradação e cores são decisão fechada — proposta de
mudança passa pela spec primeiro. "Modernizar" violando contrato não é melhoria.

---

## 3. Incidentes reais que motivaram estas regras

| # | Data | Incidente | Regras |
|---|---|---|---|
| I1 | 2026-08-21 | `Slim.exe` do PATH (build 05:45) exibiu TUI antiga; o código já tinha composer boxed, ContextRail e welcome novo. Ciclo inteiro de "docs vs realidade" desperdiçado. | R6, R7 |
| I2 | 2026-08-21 | Build com rustc 1.95 (instalação velha) + rustdoc 1.97.1 → E0514 nos Doc-tests; cache `target\debug` misto. | R8 |
| I3 | 2026-08-21 | Claim "zero referências aos números antigos" falsa: `release/README.md` ainda dizia `159 testes / 40 seções`. | R2, R11 |
| I4 | 2026-08-21 | Auditoria pré-execução (`analysis_outputs/SUMMARY.md`) contradizia o código atual em 6+ pontos; um agente a usou como base sem revalidar. | R5 |
| I5 | 2026-08-21 | Pipeline PowerShell reportou exit 1 com a suíte 100% verde; quase disparou debug falso. | R9 |
| I6 | 2026-08-21 | Bug de índice no código novo do wordmark (`WORD.len()` = nº de letras, não de linhas); pego pelo teste, não pela revisão visual. | R10 |

---

## 4. Checklist obrigatório (copiar preenchido na entrega)

```markdown
## ✅ Verificação de Entrega
- [ ] Rodei o que afirmo ter rodado (comando + saída na resposta)
- [ ] Números citados contados nesta sessão (R2)
- [ ] Caminho:linha citado foi lido nesta sessão (R3)
- [ ] cargo test --workspace verde (0 failed, 0 warnings)
- [ ] .\refresh-slim.ps1 executado, imprimiu OK: (R7)
- [ ] Docs de status/números atualizados e grep dos antigos vazio (R11)
- [ ] Incertezas e limitações declaradas explicitamente (R4)
```

Checklist incompleto = entrega rejeitada. Sem exceções.

