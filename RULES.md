# RULES.md — Protocolo anti-alucinação e anti-esquecimento

> **Protocolo de evidência consultivo, subordinado ao [`AGENTS.md`](AGENTS.md) e às instruções do usuário.**
> O AGENTS.md é a política operacional sempre carregada: escopo, git, escada de
> testes, publicação e documentação. Aqui ficam só o *como provar* (R1–R6, R8–R12),
> a memória de incidentes (§3) e o checklist de entrega (§4). Onde um item repetir
> uma obrigação do AGENTS.md, o texto dele prevalece e este arquivo não a redefine.
> Consulte as seções pertinentes à tarefa. Os incidentes da seção 3 são históricos;
> não exigem leitura nem repetição de verificações em toda mudança.

---

## 1. Verdade absoluta — NUNCA alucinar

### R1 — Nada é "feito" sem evidência da sessão
Sustente cada conclusão com evidência adequada: diff para alterações, leitura do
caminho integrado para análise estática, comando e resultado para execução.
Não apresente análise estática como prova de execução ou de comportamento live.

### R2 — Diferencie medição atual de registro histórico
Para afirmar uma contagem, versão ou medida como atual, verifique-a nesta sessão
quando ela for necessária à tarefa. Resultados anteriores podem ser citados com
data e fonte, identificados como históricos e não revalidados. Não rode a suíte
só para renovar números de documentação (escopo de testes em AGENTS.md). Quando a
suíte completa for necessária, conte sua saída real, incluindo Doc-tests, falhas e
testes ignorados.

### R3 — Caminho:linha só existe se você leu
Proibido citar `arquivo.rs:NN` como evidência sem ter lido o arquivo **nesta
sessão**. Antes de afirmar que algo "existe", "é chamado" ou "está integrado":
busque o símbolo no código. Presença de arquivo ≠ presença de wiring.

### R4 — "Não sei" é resposta; inferência disfarçada de fato é mentira
Incerteza se declara explicitamente ("não verifiquei", "inferência", "não
rodei em console físico"). Proibido preencher lacuna com afirmação plausível.
Distinga fatos verificados de inferências e hipóteses quando isso afetar a conclusão.

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

### R7 — Deploy tem escopo e identidade próprios
Quais verificações cobrem a mudança e quando publicar estão em
[`AGENTS.md`](AGENTS.md); aqui ficam o registro e a prova do deploy.

O deploy local instala uma cópia estática (`%USERPROFILE%\bin\Slim.exe`; no
macOS, `~/.local/bin/slim`) que não acompanha o checkout automaticamente.
Informe `OK:` e a identidade do executável ao declarar deploy concluído. Falha
preexistente fora do escopo alterado não bloqueia o deploy: ela consta no
relatório.

### R8 — Toolchain e variáveis do ambiente
O incidente I2 envolveu instalações Rust diferentes e variáveis herdadas inválidas.
Antes de comandos Cargo necessários, confira os caminhos efetivos e mantenha
rustc/rustdoc no mesmo toolchain. Remova ou ajuste variáveis inválidas somente
no processo da tarefa. O script de refresh (`refresh-slim.sh` no macOS,
`refresh-slim.ps1` no Windows) contém os caminhos usados no deploy local; versões
históricas não comprovam o ambiente atual. Não limpe caches por ritual:
investigue incompatibilidades e preserve trabalho existente.

### R9 — Exit code de pipeline PowerShell mente
`cargo ... 2>&1 | Select-String ...` pode reportar exit 1 com tudo verde.
Confirme com `EXIT=$LASTEXITCODE` **sem filtro** antes de diagnosticar falha.
(Incidente I5.)

### R10 — Teste novo pode ter bug no próprio teste
Teste que falha pode estar errado, não o código. Leia o assert antes de
"consertar" o código. (Incidente I6: `WORD.len()` vs `WORD[0].len()` no
wordmark — o teste pegou o bug do próprio código novo.)

### R11 — Atualize apenas a documentação afetada
Atualizar a referência vigente, o índice e os links relativos é obrigação do
[`AGENTS.md`](AGENTS.md); aqui ficam os limites. `README.md` identifica o estado
do checkout; `release/README.md` registra o deploy. Tracker e DESIGN da TUI só
mudam quando seus contratos ou estados forem afetados. Não replique contagens de
testes em vários arquivos: registre a evidência uma vez e use links. Preserve
resultados antigos com data e escopo; auditorias e planos históricos devem ser
identificados como tal. Não apague números históricos legítimos para obter uma
busca vazia.

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

## 4. Checklist de entrega por escopo

Cubra os itens abaixo de forma concisa na entrega; não é necessário copiar o
formulário. Indique explicitamente o que não se aplica. Quais e quantos checks
rodar é decisão do [`AGENTS.md`](AGENTS.md): o checklist registra evidência, não
amplia escopo.

- Evidência das alterações ou conclusões e referências lidas (R1, R3).
- Comandos realmente executados, resultados e falhas; testes de código são
  não aplicáveis a análise e documentação. A suíte completa só é necessária
  quando o risco ou o escopo a justificar ([AGENTS.md](AGENTS.md)).
- Medições atuais verificadas ou resultados históricos identificados (R2).
- Deploy: resultado do script e identidade do binário; não aplicável a mudança
  somente documental (R6, R7).
- Documentação afetada atualizada, ou não aplicável (R11).
- Incertezas e limites da validação (R4).

