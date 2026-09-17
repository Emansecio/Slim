# Release do Slim

## Deploy local atual — Consolidação pós-limpeza e auditoria (2026-09-17)

`refresh-slim.ps1` concluído com `OK:` e exit 0; build release em **3m21s**.
PATH: `C:\Users\User\bin\Slim.exe`, build **2026-09-17 04:05:50**,
`slim 0.1.0`, **17.917.952 bytes**. SHA-256 do instalado e de
`target/release/slim.exe` idênticos:
`F97E582FBA2A979D25636741AAA6BF783110CDF432D5138B55CEB70195ACF294`.

O binário reflete o checkout com a limpeza de APIs, a auditoria de
performance/corretude, o foco nos campos de texto, o catálogo ClinePass e a
cobertura adversarial — tudo ainda não commitado na data do deploy. Smoke
instalado: `--version`, exit 0. Suíte completa não repetida nesta tarefa;
a última execução registrada é a da auditoria (1.673 testes aprovados).
Console físico não validado.

## Deploy anterior — TUI em largura total (2026-09-16)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **3m03s**.
PATH: `C:\Users\User\bin\Slim.exe`, build **2026-09-16 18:23:34**,
`slim 0.1.0`, **17.884.160 bytes**. SHA-256 do instalado e de
`target/release/slim.exe`, conferidos após a cópia e idênticos:
`FE53F87EC6E46CBD7C63D86B5A5CA243A04A101D6FCDC4AD206C622ADA9E5F56`.

Remove os limites de 100/144 colunas e a centralização do workspace; conversa,
composer e rodapé usam toda a largura disponível. O cálculo de altura do
composer acompanha essa largura, inclusive com inspetor aberto.

Validação anterior ao deploy nesta tarefa: `cargo test -p slim-tui --quiet`,
499 testes aprovados e 5 ignorados. Smoke instalado: `--version`, exit 0.
A suíte completa do workspace não foi repetida. Console físico não validado.

## Deploy anterior — Tipos dos argumentos de skill (2026-09-16)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **5m06s**.
PATH resolvido: `C:\Users\User\bin\Slim.exe`, build **2026-09-16 04:14:37**,
`slim 0.1.0`, **17.875.456 bytes**. SHA-256 do instalado e de
`target/release/slim.exe`, conferidos após a cópia e idênticos:
`1C5C0E13C08F2CD42817F1FA2B9BF3267B7B472EDEDDE84AC835EE6B07050CA1`.

`skill.list` e `skill.script` rejeitam tipos inválidos, inclusive `null`,
antes de escolher listagem, fallback ou execução. Campos omitidos e strings
vazias preservam os padrões existentes; nenhuma chamada adicional ao modelo.

Validação: **8 testes pertinentes aprovados** (2 unitários, 2 do fluxo do agente
e 4 de invocação de skills), incluindo cancelamento de processos; formatação
e diff aprovados. Smoke do executável resolvido no PATH: `--version`, exit 0.
Não foi executada a suíte completa nem teste com provider real ou TUI física.

O binário foi construído do checkout atual, incluindo trabalho preexistente
não commitado. O commit desta tarefa contém somente a correção, seu teste e
a documentação; não representa sozinho todo o código usado neste binário.

## Deploy anterior — Feedback e leitura da TUI (2026-09-15)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **3m14s**.
PATH resolvido: `C:\Users\User\bin\Slim.exe`, build **2026-09-15 22:00:08**,
`slim 0.1.0`, **17.772.544 bytes**. SHA-256 do instalado e de
`target/release/slim.exe`, conferidos após a cópia e idênticos:
`6F9D3E30E3413A266BBE473B3AA8B1DBB830F65B8A7DEAFED5B5271EE93780B9`.

Inclui estados precisos de ferramentas/retry/cancelamento, fila controlável,
prévia incremental de pensamento, seleção estável, coluna de leitura compartilhada,
TODO/notificações/aprovações revistos, interface em português e confirmações
visuais breves. Preserva as alterações preexistentes do checkout.
[Validação completa, medições e limitações](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md#1-veredito-geral).
A suíte completa foi executada antes do script sem `-Test`; não foi repetida
durante o deploy. O aviso preexistente que o Clippy dos testes do core mantinha
foi corrigido em 16/09/2026 no checkout: `cargo clippy --workspace
--all-targets --offline -- -D warnings` passa com exit 0. O binário instalado
segue sendo o build de 15/09, anterior a essa correção e ao decodificador de
colagem do Windows — a mudança existe apenas no checkout e exige um novo
deploy para valer no PATH.

Smoke do instalado: `--version`, exit 0. Inicialização da TUI em PTY também
confirmada: welcome, Ctrl+P abre a paleta em português, Esc fecha a paleta e
Ctrl+C encerra/restaura o terminal com exit 0. Nenhum prompt foi enviado ao
modelo. Isso não substitui avaliação humana de fluidez em console físico nem
validação com provider comercial. Sem commit, push ou novo ZIP.

## Deploy anterior — Ferramentas, contexto e diagnóstico JSON (2026-09-15)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **3m07s**.
PATH: `C:\Users\User\bin\Slim.exe`, build **2026-09-15 19:53:38**,
`slim 0.1.0`, **17.593.856 bytes**. SHA-256 do instalado e de
`target/release/slim.exe` conferidos após a cópia, idênticos:
`90FE805ACD12496B9703E2E03804AB68F7121052003AB4BA7036716583735488`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui descrições nativas menores, exclusão de `.venv` na busca/descoberta e
diagnósticos de sintaxe de JSON nos resultados de escrita e patch. Preserva o
trabalho preexistente do checkout e os contratos de chamada. O diagnóstico usa
o conteúdo já em memória, após todas as edições do arquivo, sem rollback; aplica-se
a `.json` de até 1 MiB e não reinterpreta templates previamente inválidos. Não é
validação semântica ou comprovação de conclusão da tarefa.

Validação atual: `cargo test --workspace --no-fail-fast`, **1.613 aprovados,
zero falhas e 33 ignorados**, incluindo Doc-tests; exit 0. Antes disso, 135
testes focados passaram. A primeira execução completa detectou duas expectativas
afetadas pelas descrições/rodapé; foram corrigidas e passaram. A tentativa seguinte
e uma execução isolada falharam em
`aborting_startup_leader_does_not_strand_followers_or_shutdown` (2 inicializações
observadas, 1 esperada). Esse teste passou na execução completa final, sem mudança
no LSP: intermitência registrada, não corrigida nesta tarefa. Os avisos de testes
ignorados são preservados nos logs. Formatação dos arquivos Rust alterados e
`git diff --check` passaram. Toolchain pareado: rustc/rustdoc 1.98.0; um job.

Evidência local: `bench/luna-live/native-improvements-20260915/`, incluindo
`focused-tests.log`, `deploy.log`, `deploy-retry.log`, `lsp-recheck.log`,
`full-tests.log` e `deploy-final.log`. A suíte final antecedeu o script sem
`-Test`, sem repetição da suíte aprovada. [Protocolo do A/B](../bench/luna-live/README.md).
Sem teste manual de TUI/clipboard (não aplicável às alterações), commit ou push.

Validação live posterior: [A/B antes/depois](../bench/luna-live/README.md#resultado-do-ab-nativo),
oito pares, 16/16 aprovações externas, redução agregada de 20,3% em tokens e
21,4% em tempo; regressões individuais e limites documentados. A segunda migração
demonstrou a entrega dos diagnósticos dos dois JSONs ao modelo real.

Limpeza autorizada concluída: removidos `bench/economy-next/slim-before.exe`,
`bench/economy-next/slim-after.exe`, `bench/economy-next/numbered/slim-numbered.exe`
e o baseline temporário `bench/luna-live/native-improvements-20260915/slim-before.exe`.
**4 executáveis, 63.001.600 bytes**; ausência conferida, hashes anteriores retidos
em `removed-builds.json`. Mantidos o release atual, a cópia no PATH e as evidências.

## Deploy anterior — Seleção textual e confirmação de cópia (2026-09-14)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **2m40s**.
PATH: `C:\Users\User\bin\Slim.exe`, build **2026-09-14 21:05:57**,
`slim 0.1.0`, **17.574.912 bytes**. SHA-256 do instalado e de
`target/release/slim.exe` conferidos após a cópia, idênticos:
`5E04CB23C16CF0FADB585C2D2DC517FA7DEE91A1ECBA5DC130207FD67F15F925`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui seleção limitada ao conteúdo da conversa/inspetor, realce sem padding,
cópia da seleção atual mesmo após arrasto rápido e aviso discreto `Copiado`
somente após sucesso do clipboard. [Validação e limites](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Build com um job; suíte pertinente concluída antes do deploy, não repetida.
Sem teste manual em console físico/clipboard pessoal, provider real, commit,
push ou novo ZIP.

## Deploy anterior — Polimento complementar da TUI (2026-09-14)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **3m46s**.
PATH: `C:\Users\User\bin\Slim.exe`, build **2026-09-14 20:29:14**,
`slim 0.1.0`, **17.573.376 bytes**. SHA-256 do instalado e de
`target/release/slim.exe` conferidos após a cópia, idênticos:
`E840E62A9114393ABB1D35693A882AF0AF9148239E911C213EBC2B548A00B340`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui os polimentos de mouse/cache do inspetor, agendamento de frames,
indicador Thinking, seleção contínua de menus e identificação de espera pelo
provider. Validação pertinente concluída antes do deploy e registrada no
[tracker](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Build com um job; suíte não repetida. Sem provider real, console físico,
commit, push ou novo ZIP.

## Deploy anterior — Revisão da TUI e chamadas (2026-09-14)

`refresh-slim.ps1` concluiu com `OK:` e exit 0; build release em **3m20s**.
PATH: `C:\Users\User\bin\Slim.exe`, build **2026-09-14 19:41:02**,
`slim 0.1.0`, **17.572.352 bytes**. SHA-256 do instalado e de
`target/release/slim.exe` conferidos após a cópia, idênticos:
`344E79E36BACD9ACE557177456E5C068651B026E28E6C317EEF5980F4B1D38ED`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui o tema preto, composer com quebra visual, rodapé compacto, navegação
visível, scroll independente dos inspetores e microfeedback respeitando
movimento reduzido, além das correções de chamadas presentes no checkout.
Validação da TUI e bridge concluída antes deste deploy, registrada no
[tracker](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Build com um job; suíte não repetida. Sem validação com provider real ou
console físico, commit, push ou novo ZIP.

## Deploy anterior — Recuperação de truncamento (2026-09-14)

`refresh-slim.ps1` concluiu com `OK:` e exit 0. PATH:
`C:\Users\User\bin\Slim.exe`, build **2026-09-14 02:31:43**, `slim 0.1.0`.
Tamanho: **17.510.912 bytes**. SHA-256 do instalado e de `target/release/slim.exe`
conferidos após a cópia, idênticos:
`F8C7E3AEFB0AC610AB346F03DAAF3D30F7ED8991582769B7F2C860D1276BB62E`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui recuperação automática limitada de respostas truncadas, crescimento do
orçamento de saída e retorno ao limite original quando o provider rejeita o
aumento. Preserva respostas a perguntas e resultados concluídos; ferramentas
truncadas não são executadas. Comportamento descrito no [README](../README.md).
Build release em 3m11s. Testes pertinentes concluídos nesta sessão antes do
deploy: 77 do fluxo do agente, um do orçamento de recuperação e dois da mensagem
de parada; todos passaram. Suíte completa não repetida. Sem validação com
provider real ou console físico, commit, push ou novo ZIP.

## Deploy anterior — Whitespace e recriação após exclusão (2026-09-13)

`refresh-slim.ps1` concluiu com `OK:` e exit 0. PATH:
`C:\Users\User\bin\Slim.exe`, build **2026-09-13 20:38:47**, `slim 0.1.0`.
Tamanho: **17.472.512 bytes**. SHA-256 do instalado e de `target/release/slim.exe`
conferidos após a cópia, idênticos:
`560BF0AC9E019756FFCEF4BF3DB3215591BE1AB71BD2E43370B1D8EB26458F63`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui preservação de whitespace no adaptador Chat e invalidação da observação
antiga para recriar um arquivo excluído com `expected` omitido ou `null`.
Build release com um job. Testes pertinentes já concluídos antes do deploy;
suíte não repetida. Sem provider real, console físico, commit, push ou novo ZIP.

## Deploy anterior — Correções das auditorias (2026-09-13)

`refresh-slim.ps1` concluiu com `OK:` e exit 0. PATH:
`C:\Users\User\bin\Slim.exe`, build **2026-09-13 12:45:13**, `slim 0.1.0`.
Tamanho: **17.472.000 bytes**. SHA-256 do instalado e de `target/release/slim.exe`
conferidos após a cópia, idênticos:
`BC2C49B8784C35FF56F26C45ABEF6E9512DB198AC4D4F23C68871B8486E1C073`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Build release do checkout com as correções das auditorias, incluindo publicação
recuperável, preservação de edições externas, credenciais, cancelamento de
trabalhos nativos, UTF-8, espaços, SSE e histórico da TUI após falha.
Testes pertinentes já concluídos antes do deploy; suíte não repetida nesta
instalação. Sem teste com provider real, console físico, commit, push ou novo ZIP.

## Deploy anterior — MCP Windows + overlay `/mcp` (2026-09-12)

`refresh-slim.ps1` concluiu com `OK:` e exit 0. PATH:
`C:\Users\User\bin\Slim.exe`, build **2026-09-12 22:46:34**, `slim 0.1.0`.
Tamanho: **17.156.096 bytes**. SHA-256 do instalado e de `target/release/slim.exe`
conferidos após a cópia:
`F994900E00C742CA4BADFCDC9ECD2CDD96F903A3E6FE5B2ADA4ABDEBFB9A4EBE`.
Smoke pelo PATH: `--version`, exit 0.

Corrige o spawn stdio de `npx` no Windows (shim `#!` → `.cmd`, sem os error 193)
e a overlay `/mcp` (comando/erro em linhas próprias; toast oculto com a lista
aberta). Sem commit, push, ZIP ou gate ConPTY nesta atualização.

## Deploy anterior — Pensamento refinado na TUI (2026-09-10)

`refresh-slim.ps1` concluiu com `OK:` e exit 0. PATH:
`C:\Users\User\bin\Slim.exe`, build **2026-09-10 16:39:46**, `slim 0.1.0`.
Tamanho: **15.742.464 bytes**. SHA-256 do instalado e de `target/release/slim.exe`
conferidos após a cópia:
`3216F880F12905678EA55CF42C2A1E45FA23DC8E5BCE758253F702D0EC3220E3`.
Smoke pelo PATH: `--version`, exit 0.

Pensamento com pulso contextual de uma célula, disclosure para detalhes e
texto alinhado. Sem timer novo ou duração por bloco inventada.
[Validação e limites desta revisão](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
Não houve nova execução do gate ConPTY, console físico, commit, push ou ZIP.

## Deploy anterior — Autoria clara na TUI (2026-09-10)

`refresh-slim.ps1` concluiu com `OK:` e exit 0. Instalado em
`C:\Users\User\bin\Slim.exe`, build **2026-09-10 12:59:07**, versão `slim 0.1.0`.
Tamanho: **15.740.928 bytes**. SHA-256 do PATH e de `target/release/slim.exe`
conferidos após a cópia, idênticos:
`12BE3A7405CE82D81E1B87F6B3809C368E6EEF45DB005B2365186AEAC062FDA9`.
Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Respostas recebem cabeçalho `Slim` persistente e respiro após o pensamento;
faixa `You` mais suave. [Validação do refinamento](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
O gate ConPTY continua pendente; não houve validação em console físico,
commit, push ou ZIP de distribuição.

## Deploy anterior — TUI minimalista (2026-09-10)

Deploy autorizado após a implementação da TUI. `refresh-slim.ps1` concluiu
com `OK:` e exit 0, sem repetir a suíte já executada nesta tarefa.

- PATH: `C:\Users\User\bin\Slim.exe`.
- Build: **2026-09-10 12:45:23**, versão `slim 0.1.0`.
- Tamanho: **15.740.928 bytes**.
- SHA-256 do instalado e de `target/release/slim.exe`, conferidos após a cópia:
  `7F7770127434374617B12DE3F60A74D9C586D54C8E8D131C393F8F7821733FA1`.
- Smoke pelo executável resolvido no PATH: `--version`, exit 0.

Inclui a coluna central, paleta quente, footer responsivo e pulso de atividade
do checkout. [Testes anteriores ao deploy e limites de validação](../Documentações%20-%20Projeto/AUDIT-SLIM-TUI-TRACKER.md).
O deploy não resolve o gate ConPTY pendente nem comprova console físico.
ZIP de distribuição, commit e push não fazem parte desta atualização local.

## Deploy anterior — Command Code / DeepSeek V4.1 Flash (2026-09-10)

`refresh-slim.ps1` concluiu com `OK:`. Instalado em
`C:\Users\User\bin\Slim.exe`, build **2026-09-10 03:41:58**, versão `slim 0.1.0`.
Instalado e `target/release/slim.exe`: **15.693.312 bytes**, SHA-256 idêntico
`7A359629F7D855F14D2122CFEB27701EF952EDFD0E00D6B8AEEC6E5BC9F14AC9`.

Mudança limitada ao Command Code: padrão e fallback incluem
`deepseek/deepseek-v4.1-flash` (1.000.000 tokens); parser aceita `:` em IDs
oficiais. Antes, entradas `:free` invalidavam o catálogo inteiro e impediam
V4.1 de aparecer. Modelos anteriores e credenciais de outros providers preservados.
[Uso, plano GOAT e fontes oficiais](../README.md#command-code--deepseek-v41-flash).

Evidência atual:
- Regressão reproduzida antes da correção: refresh falhou com
  `Command Code model id is invalid`.
- `cargo test -p slim-core --test command_code_provider`: 8 passed.
- `cargo test -p slim-cli --test command_code_catalog`: 3 passed.
- `cargo test -p slim-tui --lib command_code`: 3 passed.
- `cargo test -p slim-cli --test tui_bridge known_provider_models_use_their_catalog_context_windows`:
  1 passed, incluindo turno TUI em localhost com V4.1 e contexto correto.
- rust-analyzer: zero diagnósticos nos cinco arquivos Rust alterados.
- `CommandCodeCatalog::production().refresh()` consultou API pública real:
  **69 entradas**, V4.1 com nome/ID/contexto confirmados; releitura do cache
  igual ao snapshot live. Cache local atualizado. Helper temporário removido;
  primeira compilação do helper falhou por conversão de erro E0277, corrigida
  no próprio helper antes da execução bem-sucedida.
- `--version` pelo deploy: exit 0. Smoke autenticado pelo instalado, com
  `--provider command-code --model deepseek/deepseek-v4.1-flash --effort high`,
  parou com exit **20**: nenhuma credencial Command Code cadastrada.

**Pendente:** `/login command-code` e teste real de geração após cadastrar chave.
Não há prova de acesso ao plano da conta, geração ou ferramentas live nesse
provider. Suíte completa, console físico e imagens não exercitados;
ZIP de distribuição não aplicável a esta instalação local.

## Deploy anterior — OpenCode Go / DeepSeek V4.1 Flash (2026-09-10)

`refresh-slim.ps1` concluiu com `OK:` e instalou o suporte já presente no checkout
em `C:\Users\User\bin\Slim.exe`, build de **2026-09-10 03:30:26**.
Executável instalado e `target/release/slim.exe`: **15.693.312 bytes**, SHA-256
`111A4183E3885EF896B950AEA5FD955A5741B4313C2D3823F90E44D367F7AAF5`.
`--version`: `slim 0.1.0`, exit 0.

OpenCode Go publica **DeepSeek V4.1 Flash** com ID `deepseek-flash`, confirmado
no [catálogo público](https://opencode.ai/zen/go/v1/models) e nos
[endpoints oficiais](https://opencode.ai/docs/go/#endpoints) nesta sessão.
O executável anterior era de 01:26:16, anterior ao suporte no checkout.
Reabra o Slim e use `/models` para atualizar o catálogo e selecionar a entrada;
`deepseek-v4-flash` permanece como V4 anterior.

Validação desta atualização (rustc/rustdoc 1.98.0):
- `cargo test -p slim-core --test opencode_go_provider`: 18 passed.
- `cargo test -p slim-cli --test opencode_go_catalog`: 10 passed, incluindo
  refresh e releitura do cache com `deepseek-flash`.
- `cargo test -p slim-cli --lib opencode`: 3 passed.
- `cargo test -p slim-cli --lib deepseek_flash_catalog`: 1 passed; evento da TUI
  mantém nome V4.1 e ID canônico separados de V4.
- rust-analyzer: zero diagnósticos nos dois arquivos Rust alterados nesta sessão.
- Smoke **live** em pasta temporária vazia, pelo executável instalado:
  `Slim --headless --read-only --provider opencode-go --model deepseek-flash --effort low --prompt "Responda apenas OK. Nao use ferramentas."`
  respondeu `OK`, exit 0.

Suíte completa não repetida: escopo de catálogo/roteamento, sem nova alteração
funcional nesta sessão. Console físico, imagens, tarefas com ferramentas e
outros esforços não exercitados live. ZIP de distribuição não regenerado.

## Registros anteriores — históricos, não representam o deploy atual

Pacote Windows x64/MSVC do Slim `0.1.0`. O ZIP é determinístico e contém
exatamente uma entrada, `slim.exe`; não inclui `auth.json`, `.slim`, sessões,
artefatos, prompts, documentação ou segredos. Os hashes publicados ficam em
`manifest.json` e `SHA256SUMS.txt`.

> **Status do pacote (2026-08-21):** os artefatos anteriores
> (`slim-0.1.0-windows-x64.zip`, `manifest.json`, `SHA256SUMS.txt`) foram
> **removidos** por serem de um binário anterior aos últimos slices do tracker
> TUI. Regere o pacote com `python3 release/build_release.py` a partir de um
> `target/release/slim.exe` fresco antes de distribuir.

> **Status de implementação:** este artefato é o checkpoint `0.1.0` de
> integração parcial. `Slim` abre a TUI normal mesmo deslogado; `/login` conecta
> OAuth nativo Claude Pro/Max ou ChatGPT Plus/Pro e chave mascarada OpenCode Go.
> `Slim --headless` usa o caminho headless; ambos usam provider/loop/tools reais.
> No headless text, `--verbose` acrescenta uma timeline redigida de tools;
> não pode ser combinado com `--jsonl`.
> OpenCode Go roteia 23 modelos documentados por Chat Completions, Responses ou
> Messages e usa catálogo público bounded com cache/fallback offline. O harness v2 inclui resume/
> recovery explícitos e `RuntimeCapabilityBridge` durável para Skill, MCP local
> selecionado, child e Todo/Plan/Goal. O pacote ainda não registra automaticamente
> essas capabilities no provider loop/CLI/TUI, não inclui transporte MCP externo,
> processo Skill/child real ou uma v1 completa.

## Verificação reproduzível

Na raiz do workspace, com `target/release/slim.exe` já compilado:

```powershell
python3 release/build_release.py
python3 release/build_release.py
sha256sum -c release/SHA256SUMS.txt
7z l release/slim-0.1.0-windows-x64.zip
.\target\release\slim.exe --version
.\target\release\slim.exe --help
.\target\release\slim.exe --headless --unknown-option
```

As duas execuções devem produzir o mesmo SHA-256 do ZIP. A listagem deve
mostrar somente `slim.exe`, com timestamp fixo `1980-01-01 00:00:00`. O builder
usa apenas a biblioteca padrão do Python, resolve todos os caminhos a partir da
raiz de `build_release.py`, escreve somente em `release/` e não cria artefato
temporário fora de `release/`.

O binário deve retornar exit `0` para `--version` e `--help`, e exit `30` para
opção desconhecida. A verificação de bytes/nome do ZIP não encontra os markers
secretos das fixtures.

## Escopo e limitações

O checkout inclui a TUI simplificada e falhas do provider persistentes na tela e
na sessão. `/resume` aceita turnos manuais cancelados e restaura respostas salvas
sem reexecutar ações. O loop permite até duas novas tentativas para falhas transitórias antes
de resposta/ferramenta, preservando resultados anteriores, cancelamento e orçamento.
Conclusão normal vazia ou só com reasoning é erro explícito.
Inclui as nove correções de confiabilidade e a decisão `--abandon-pending` com
`--recover`, sem replay de ferramentas. [Uso e limites](../README.md#continuação-de-sessões-com-ferramentas).
Passou em 1211 testes / 0 failed / 3 ignored / 74 suítes
(`refresh-slim.ps1 -Test` / `cargo test --workspace`, Cargo offline, 2026-09-06).
O release de 2026-09-06 foi instalado por `refresh-slim.ps1 -Test`, com `OK:`;
`target/release/slim.exe` tinha 15.143.936 bytes e SHA-256
`0D1CC3690B410B0DA945629AC11DC39569D1244683929E7A51216322F90C52E1`.
Deploy local histórico (2026-09-08 23:11:07): `target/release/slim.exe`
tinha 15.633.920 bytes e SHA-256
`A09EEF290870139273142BF8CD63F757B537C471D9F7B35379234EF370C859B7`.
Deploy local histórico (2026-09-09 19:06:24): `target/release/slim.exe`
tinha 15.665.664 bytes e SHA-256
`9525940A32F4F1F21A7D2AD1CCF166AA92E98EF9A4D95853C90898B98D23AD32`.
O deploy local atual (2026-09-09 19:40:51) foi `.\refresh-slim.ps1` (sem `-Test`)
após o retry robusto de provider: copiou `target/release/slim.exe` (15.664.640 bytes, SHA-256
`8B7EEB358BBA40E41EC4958215F9602D7C23AE8A1BC65117C2BCA166072442A9`)
para `C:\Users\User\bin\Slim.exe`. Os dois arquivos têm SHA-256 idêntico.
`--version` retornou `slim 0.1.0`, exit 0, pelo executável instalado.

Validação comercial anterior (Muse, não repetida nesta atualização):
O checkout foi instalado com `refresh-slim.ps1` após esse gate. Testes sintéticos
no Go real com Muse 1.3/xhigh, pelo debug e pelo executável instalado, responderam
`OK`, exit 0. O HTTP 500 não ocorreu; tarefas completas no Go não foram exercitadas.

O checkout validado para o binário instalado registra 1211 passed / 0 failed / 3 ignored em 74 suítes; a fixture
localhost exercita o turno pela API TUI, sem provider live. Em verificação anterior,
o smoke ConPTY emitiu só o probe `ESC[6n`, sem
frame; a validação física completa de terminal, IME, mouse e clipboard permanece
não observada.

Contexto inicial distribuído por pasta e prazos TCP/TLS separados da espera de cabeçalhos. A bateria anterior registrada no benchmark terminou com 32/32 pares e 64/64 braços aprovados: Luna -10,76% tokens e DeepSeek -21,31%, com vantagem em 6/8 e 7/8 cenários, respectivamente. Os oráculos passaram integralmente; a vantagem em qualquer provider/tarefa permanece não demonstrada. [Código e benchmark](../bench/luna-live/README.md#contexto-equilibrado-e-prazos-http).

No deploy atual, `target\release\slim.exe` e `C:\Users\User\bin\Slim.exe` têm
15.633.920 bytes e SHA-256 idêntico
`A09EEF290870139273142BF8CD63F757B537C471D9F7B35379234EF370C859B7`;
`slim --version` retorna `slim 0.1.0` com exit `0`.
`cargo clean --offline --profile dev` removeu 36.190 arquivos / 35,3 GiB do cache debug.
`C:\Users\User\bin\Slim.exe.old` foi apagado.

O ZIP de distribuição não foi regenerado nesta atualização do executável local.

Quick wins no core: edição CRLF compatível com trechos de leitura, falhas com
localização e schemas de edição mais explícitos. [Relatório e evidência](../analysis_outputs/QUICK-WINS-AGILIDADE-NATIVA-SLIM.md).

Build/deploy local de 2026-09-08 23:11:07: `cargo build --release -p slim-cli`
(rustc/cargo 1.98.0) e cópia para o PATH; `slim --version` = `slim 0.1.0`.
O gate anterior de 2026-09-06 permanece histórico; [Astra, Normal/Fast e validação offline](../README.md#openai-codex--gpt-6-astra).
O gate mantém a [otimização dos testes e as cinco etapas](../analysis_outputs/OTIMIZACAO-TESTES-E-BUILD-SLIM.md).

Esses testes comprovam componentes/contratos, headless, a bridge TUI central e o
capability bridge offline, não a integração de todas as capabilities da v1. O transporte HTTP compartilhado está ativo nos construtores normais; o cache
local de respostas é opt-in e permanece desligado no produto. Adapter e
autenticação seguem isolados por request. O cache nativo de prompt depende do wire.

`auth.json` continua opcional e fail-closed para leitura; a TUI grava/remove a
entrada OpenCode Go por temp exclusivo, lock, ACL e replace atômico, preservando
providers irmãos. Precedência: `SLIM_API_KEY` > `OPENCODE_API_KEY` > arquivo. No Windows, a
implementação usa handle `windows-sys` e DACL protegida com allowlist exata do
owner atual, usuário atual, `SYSTEM` e `Administrators`, incluindo teste de
caminho Unicode. Symlink, arquivo não regular, ACL divergente ou schema inválido
falham; headless não cria arquivo ausente, enquanto login TUI explícito pode
criá-lo com segurança. Credenciais não entram em cache, sessão,
manifest ou ZIP.

Continuação de sessões: novos arquivos `--session` usam v2; CLI e bridge TUI
retomam conversas com chamadas/resultados e novas ferramentas, preservando modo,
orçamentos e raiz salva. Interrupções ambíguas ficam bloqueadas sem replay; v1
não é migrado. [Validação offline e limites](../README.md#continuação-de-sessões-com-ferramentas).

A retomada TUI também restaura ferramentas em blocos `history` recolhidos; Enter
consulta argumentos e resultados salvos, sem replay ou inferência de sucesso.
