# Provider OpenCode Go no Slim

**Data:** 2026-08-24  
**Status:** design aprovado; auto-revisão concluída; aguardando revisão final do arquivo  
**Referências externas:**

- documentação OpenCode Go: `https://opencode.ai/docs/pt-br/go/`;
- catálogo público: `https://opencode.ai/zen/go/v1/models`;
- instalação Pi local usada somente como referência de formatos e metadados, nunca como dependência do Slim.

## 1. Objetivo

Adicionar integração completa do OpenCode Go ao Slim:

- provider próprio em headless e TUI;
- autenticação por `OPENCODE_API_KEY`, `SLIM_API_KEY` ou chave cadastrada pela TUI;
- `/login` com entrada mascarada e persistência segura;
- `/models` com refresh assíncrono do catálogo público;
- seleção de modelo por ID estável;
- roteamento por protocolo documentado: OpenAI Chat Completions, OpenAI Responses ou Anthropic Messages;
- fallback offline sem afirmar suporte ou limites não comprovados.

O modelo padrão será `deepseek-v4-flash`.

## 2. Evidência observada em 2026-08-24

A documentação oficial descreve 23 modelos e informa o endpoint adequado para cada um. O endpoint público `/v1/models` retornou 29 IDs no mesmo dia. Os seis IDs adicionais não tinham protocolo documentado na página.

O endpoint de modelos respondeu sem autenticação. Portanto ele confirma disponibilidade, mas **não valida a API key**. A credencial só pode ser confirmada por um request real de inferência; o Slim não fará request faturável artificial apenas para validar login.

A instalação Pi local contém um catálogo OpenCode Go útil como referência, mas seus snapshots embutido e atualizado divergem em alguns protocolos e conjuntos de modelos. O Slim não lerá arquivos do Pi e não herdará sua atualização automática. A documentação oficial e os testes de wire do Slim serão autoridades.

## 3. Abordagens consideradas

### A. Catálogo híbrido e roteamento explícito — escolhida

Manter tabela embutida com protocolo documentado e metadata conhecida. Ao abrir `/models`, buscar o catálogo público e intersectá-lo com essa tabela. Cachear apenas o último catálogo válido.

**Vantagens:** funciona offline, não adivinha protocolo, não gasta quota com probing e permite atualização de disponibilidade.

### B. Tratar todo modelo como Chat Completions — rejeitada

É menor, mas quebra modelos documentados para Responses ou Messages, inclusive tools, reasoning e usage.

### C. Descobrir protocolo por tentativas — rejeitada

Faria requests pagos, introduziria latência e produziria resultados ambíguos em falhas temporárias ou de autenticação.

## 4. Arquitetura

### 4.1 Provider lógico

Adicionar `ProviderKind::OpenCodeGo`. Esse valor identifica seleção, autenticação, saída headless e configuração do usuário. O protocolo de wire é derivado do modelo e continua usando os normalizadores existentes.

Defaults:

| campo | valor |
|---|---|
| provider | `opencode-go` |
| base | `https://opencode.ai/zen/go/v1` |
| catálogo | `https://opencode.ai/zen/go/v1/models` |
| modelo | `deepseek-v4-flash` |
| variável de chave | `OPENCODE_API_KEY` |

Aliases aceitos para provider: `opencode-go`, `opencode_go` e `go`. O nome serializado e exibido permanece `opencode-go`.

### 4.2 Registro de modelos

Criar um registro pequeno no core com:

```rust
pub enum OpenCodeApi {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

pub struct OpenCodeModel {
    pub id: &'static str,
    pub name: &'static str,
    pub api: OpenCodeApi,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub accepts_images: bool,
    pub reasoning_levels: &'static [&'static str],
}
```

Protocolos seguem a tabela oficial atual:

- **Responses:** `grok-4.5`, `gpt-5.6-luna`, `muse-spark-1.2-contributor`;
- **Anthropic Messages:** `minimax-m3`, `minimax-m2.7`, `minimax-m2.5`, `qwen3.8-max`, `qwen3.7-max`, `qwen3.7-plus`, `qwen3.6-plus`;
- **Chat Completions:** demais modelos documentados: `glm-5.3`, `glm-5.2`, `glm-5.1`, `kimi-k3`, `kimi-k2.7-code`, `kimi-k2.6`, `deepseek-v4-pro`, `deepseek-v4-flash`, `deepseek-v4-flash-vision-exp`, `mimo-v2.5`, `mimo-v2.5-pro`, `hy3`, `ox-alpha-free`.

Campos de contexto/output sem fonte confiável ficam `None`. A TUI mostra desconhecido e o runtime usa configuração explícita/default conservador existente; nunca apresenta valor estimado como metadata exata. Quando o cap documentado existe, o cap efetivo é o menor entre metadata e override explícito; sem metadata, vale o override/default local existente.

Um ID retornado pelo catálogo mas ausente do registro aparece como indisponível. Atualizar disponibilidade não altera protocolo, contexto ou capabilities.

### 4.3 Adapters

- Chat Completions reutiliza `OpenAiCompatibleAdapter` com URL `/chat/completions`.
- Messages extrai/reusa serializer e parser genéricos do `AnthropicAdapter`; o wrapper OpenCode usa `/messages` e Bearer comum, sem `x-api-key`, headers beta ou identidade Claude OAuth.
- Responses extrai o serializer/parser genérico hoje acoplado ao adapter Codex. O wrapper OpenCode usa `/responses`, Bearer comum e não envia headers `chatgpt-account-id`, `originator` ou `OpenAI-Beta` específicos da assinatura.
- `OpenAiCodexAdapter` preserva seu contrato atual e continua sem `max_output_tokens` no wire.

Não criar adapter por modelo.

## 5. Catálogo e cache

### 5.1 Refresh

Abrir `/models` com OpenCode Go ativo:

1. materializa imediatamente cache válido ou fallback embutido;
2. dispara uma única busca em background para `/v1/models`;
3. valida status, tamanho e schema;
4. intersecta IDs recebidos com registro suportado;
5. substitui a lista sem fechar overlay;
6. preserva seleção pelo ID, nunca pelo índice.

Startup e execução headless normal não consultam catálogo. O usuário pode informar diretamente qualquer modelo **registrado** com `--model` mesmo offline.

### 5.2 Bounds

- timeout total: 15 segundos;
- redirects desabilitados;
- resposta máxima: 1 MiB;
- máximo: 256 entradas;
- ID máximo: 128 bytes;
- IDs aceitam somente ASCII alfanumérico, `.`, `_`, `/` e `-`;
- duplicatas por ID são rejeitadas;
- schema desconhecido não substitui cache válido.

### 5.3 Persistência

Cache público em `~/.slim/opencode-go-models.json`, versão própria, escrito atomicamente. Symlink, diretório ou arquivo acima do limite são rejeitados. Cache não contém chave, headers, prompts, responses nem usage.

Falha de refresh:

- com cache válido: mantém cache e mostra aviso curto;
- sem cache: usa fallback embutido e mostra estado offline; modelos documentados do fallback permanecem selecionáveis;
- nunca remove modelo ativo silenciosamente;
- após refresh válido, modelo ausente fica marcado indisponível para nova seleção.

## 6. Autenticação

### 6.1 Precedência

1. `SLIM_API_KEY`;
2. `OPENCODE_API_KEY`;
3. `providers.opencode-go.api_key` em `~/.slim/auth.json`.

Variáveis de ambiente nunca são copiadas para disco.

### 6.2 TUI

`/login` ganha terceira opção, **OpenCode Go**. Ao selecioná-la:

- abre campo modal mascarado;
- caracteres entram em `SensitiveText`, cujo `Debug` permanece redigido;
- `Backspace` remove um caractere Unicode;
- `Enter` rejeita valor vazio e solicita persistência;
- `Esc` limpa estado e não persiste;
- paste é permitido, mas nunca entra em composer, history, transcript ou telemetry.

Adicionar comando tipado específico, por exemplo `SaveApiKey`, em vez de transportar segredo em notification/texto comum.

A chave é salva sem falsa validação remota. O primeiro request real que retornar `401/403` mantém erro sanitizado e oferece reabrir troca de chave.

### 6.3 Auth store

Estender schema v1 existente:

```json
{
  "version": 1,
  "active_provider": "opencode-go",
  "providers": {
    "opencode-go": { "api_key": "..." }
  }
}
```

Reusar writer atômico e lock do auth store existente. Preservar entradas OAuth/API de outros providers. No Windows, aplicar ACL restrita já exigida pelo loader. Rejeitar symlink e diretório antes de leitura ou escrita.

`/logout`:

- remove chave persistida OpenCode Go quando ela é a fonte ativa;
- desconecta sessão atual;
- se `SLIM_API_KEY`/`OPENCODE_API_KEY` existir, informa que variável continua tendo precedência e não tenta alterá-la.

## 7. Seleção de provider e modelo

### 7.1 Headless

Exemplo:

```powershell
$env:OPENCODE_API_KEY = "..."
slim --provider opencode-go --model deepseek-v4-flash "revise este código"
```

A CLI deriva:

- protocolo e endpoint pelo registro;
- context window conhecido pelo modelo, salvo override explícito;
- output cap conhecido, limitado por override explícito válido;
- reasoning somente nos níveis declarados;
- suporte a imagem antes de abrir arquivo ou enviar request.

Modelo não registrado falha antes da rede com mensagem indicando `/models`.

### 7.2 TUI

O estado de autenticação passa a distinguir OpenCode Go dos dois logins OAuth. `/models` usa opções dinâmicas com ID, label, disponibilidade, protocolo e níveis de reasoning. A lista Codex fixa continua funcionando pelo mesmo overlay, sem depender do catálogo Go.

Escolher modelo atualiza atomicamente provider request, model, protocolo, context window e effort compatível. Nenhum run em andamento muda de modelo; seleção durante run aplica-se ao próximo prompt.

## 8. Fluxo de dados

```text
/login OpenCode Go
  -> SensitiveText mascarado
  -> SaveApiKey
  -> auth.json atômico
  -> provider ativo

/models
  -> cache/fallback imediato
  -> GET público /v1/models em background
  -> validação/bounds/interseção
  -> ModelCatalogLoaded
  -> seleção por ID

prompt
  -> ProviderKind::OpenCodeGo
  -> OpenCodeModel registrado
  -> adapter por protocolo
  -> normalizador existente
  -> SessionEvent/UiEvent existentes
  -> TUI/headless/durabilidade
```

## 9. Erros e segurança

- Toda URL de provider/catalog usa HTTPS oficial por default; overrides existentes continuam explícitos e testáveis com localhost.
- Headers de autorização, query secrets e API key entram na lista de redaction antes de parsing/streaming.
- Erros remotos permanecem bounded e sanitizados.
- Catalog fetch não segue redirect nem confia em metadata remota para escolher protocolo.
- Nenhum segredo aparece em `Debug`, snapshots, goldens, JSONL, cache ou texto de erro.
- Falha de persistência não mantém UI como autenticada.
- Falha de catálogo não impede request com modelo registrado já selecionado.
- `401/403` não apaga chave automaticamente; evita perda por falha transitória ou endpoint errado.

## 10. TDD e testes

Cada comportamento entra por RED observado antes do código.

### Core/provider

- aliases e serialização de `ProviderKind::OpenCodeGo`;
- lookup dos 23 modelos documentados;
- IDs extras ficam indisponíveis;
- request Chat Completions, Responses e Messages com tools;
- imagens aceitas/rejeitadas por capability;
- reasoning level compatível;
- usage parcial/final, stop, truncation e erro remoto;
- cancelamento e redaction em chunks fragmentados;
- Codex subscription permanece sem regressão.

### Auth/catalog CLI

- precedência `SLIM_API_KEY` → `OPENCODE_API_KEY` → arquivo;
- save/remove preservando OAuth e providers irmãos;
- ACL, atomicidade, lock e rejeição de symlink;
- catálogo válido, oversized, duplicado, schema inválido, timeout e redirect;
- cache válido, fallback e modelo removido;
- headless fixture local para cada protocolo.

### TUI

- terceira opção de login;
- máscara, paste, Backspace, Enter e Esc;
- segredo ausente de `Debug`, transcript e events comuns;
- `/models` mostra fallback antes do refresh;
- refresh preserva seleção por ID;
- IDs indisponíveis não podem ser confirmados;
- seleção muda próximo request sem alterar run ativo;
- `401/403` oferece troca de chave sem expor valor.

## 11. Execução em slices

1. **Registro/provider headless:** enum, metadados e três rotas de wire.
2. **Auth store:** resolução, save/remove e segurança.
3. **Catálogo:** fetch bounded, cache/fallback e eventos.
4. **TUI:** login mascarado, provider ativo e model picker dinâmico.
5. **Integração/docs/deploy:** fixtures E2E, docs vivas e binário PATH.

Cada slice: atualizar spec normativa primeiro, escrever RED, confirmar motivo, implementar mínimo, executar focused GREEN, revisar diff e só avançar sem falha/warning/bloqueador.

## 12. Documentação viva

Atualizar na mesma tarefa:

- `Documentações - Projeto/DESIGN-SLIM-TUI.md` antes de comportamento novo;
- `Documentações - Projeto/AUDIT-SLIM-TUI-TRACKER.md` §5 para bug novo e §7 por slice;
- `README.md`;
- `Documentações - Projeto/README.md`;
- `Documentações - Projeto/PLANO-IMPLEMENTACAO.md`;
- `release/README.md`;
- números de testes em todos os arquivos que os citam.

## 13. Fora de escopo

- depender de arquivos/configuração do Pi;
- probing pago para descobrir protocolo;
- aceitar automaticamente IDs sem protocolo documentado;
- sincronizar preços ou limites de assinatura em cada startup;
- alterar variáveis de ambiente no logout;
- armazenar chave no cache de modelos;
- criar adapter separado para cada modelo;
- mudar contratos dos providers existentes além da extração necessária do adapter Responses genérico.

## 14. Critérios de aceite

- `--provider opencode-go` funciona com os três protocolos documentados;
- OpenCode Go aparece em `/login` e aceita chave mascarada persistida com segurança;
- env/config e TUI obedecem precedência definida;
- `/models` abre imediatamente e atualiza catálogo em background;
- somente modelos do registro documentado ficam selecionáveis; refresh válido também exige presença no catálogo, enquanto cache/fallback sustenta operação offline;
- falha offline usa cache/fallback sem inventar suporte;
- modelo selecionado determina endpoint, context window e capabilities sem heurística;
- chave não vaza em nenhuma superfície testada;
- providers existentes não regridem;
- `cargo test --workspace` termina com 0 falhas e 0 warnings;
- `refresh-slim.ps1 -Test` imprime `OK:`;
- `C:\Users\User\bin\Slim.exe` corresponde ao release final.
