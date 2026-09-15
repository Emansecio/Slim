# Economia de tokens — ciclo de contrato shell, 05/09/2026

## Protocolo fixado antes das chamadas

Hipótese: exemplos explícitos distinguindo programa/argumentos literais de script
PowerShell evitam recuperação por `args=[]` com comando inteiro e heredoc Bash.
A bateria anterior, recalculada dos traces em `balanced-context-native-summary.json`,
contém três erros desse tipo entre cinco falhas Slim. JSON inválido e SQL incorreto
exigem correção e não são tratados como custo dispensável. Leituras de requisitos,
evidências e verificações afetadas também não são removidas.

Uma única alteração candidata: descrição de `shell` no schema compartilhado. Mantém
args, parser, execução, permissões, timeout e captura de saída. Não muda provider,
modelo, esforço, prompt de sistema nem regras baseadas nos cenários. O overhead
adicional da descrição será contado em todas as requisições.

Três braços: Slim anterior compilado do checkout inicial sujo; candidato do mesmo
checkout com essa única alteração; Pi nativo instalado. Versões e SHA256 estão em
[manifest.json](manifest.json). Nenhum reset/checkout/stash/commit. Os dois executáveis
Slim preservados aqui são artefatos da comparação, não instalações alternativas.

Quatro cenários, duas rodadas, dois modelos: **48 tentativas**, das quais 12 são
conversas de dois turnos, total máximo de 60 processos. Em cada cenário a ordem
gira entre before/after/pi e é invertida na segunda rodada. Processos do mesmo
provider são sequenciais; as duas rotas podem progredir em paralelo. Sem seed
disponível e sem limpeza de cache remoto; apresentar entrada sem cache, leitura/
escrita de cache e saída. Caches remotos aquecidos e transporte nativo são limitações.

Rotas: Luna via openai-codex; DeepSeek V4 Flash via opencode-go. High em ambos,
normal no Codex. Ferramentas nativas preservadas; Pi sem extensões pessoais, com
observador passivo de payload/eventos. Sem proxy nem rota compatível substituta.

| Cenário | Origem | Aceitação definida antes de medir |
|---|---|---|
| config_migration | conhecido | JSON semanticamente exato; metadados/listas preservados; v2 byte-idêntico |
| ledger_audit | conhecido | CSV com aspas/Unicode, última versão, status, estornos e fonte preservada |
| source_audit | novo, não usado para ajustar o candidato | snapshot real: 215 arquivos / 109549 linhas / 3985909 bytes; cinco fatos corretos, citações de código e teste verificáveis; fontes intactas; explicação de paginação revista no trace |
| cache_conversation | novo, não usado para ajustar o candidato | LRU, Map keys, undefined, validação, caller e recência; segundo turno na mesma sessão adiciona TTL com relógio injetado, fronteiras, expiração antes de eviction, sem regressão nem sleeps; modelo deve executar testes originais e próprios |

Fixtures, prompts e oráculos estão congelados em `frozen/` e no manifest antes
da primeira execução. A auditoria externa usa o oráculo original e hashes de todos
os arquivos protegidos, não apenas exit code do agente. A implementação das tarefas
é real, feita pelo modelo; o cenário grande é um snapshot real de Rust, os demais
são fixtures funcionais pequenos. Não representam um projeto grande compilado.

Métrica principal: soma de entrada sem cache + cache read + cache write + saída,
por tarefa completa, incluindo os dois turnos, erros, retries e correções. Reasoning
é subconjunto de saída e não será somado novamente; indisponível permanece null.
Falha/uso desconhecido permanece no conjunto com total null e subtotal conhecido
separado. Não substituir tentativas. Desempenho agregado completo só pode ser afirmado
quando os totais forem conhecidos. Preço faturado não é inferido dos tokens.

Aceitar a candidata como economia demonstrada nesta amostra somente com qualidade
preservada, redução do total contra before, análise por modelo/cenário e evidência
dos percursos. Relatar dispersão e pares, sem concluir significância/universalidade
com duas rodadas. Se não houver benefício sustentado, descartar a alteração; não
repetir a bateria até obter resultado favorável. Casos novos permanecem no agregado.

Validação local já executada antes das medições: `cargo test -p slim-core --test
native_tool_recovery --test tool_contracts`: 37 passed / 0 failed; build release
do baseline e candidato concluídos. Gate completo e refresh só na decisão final.

O [ciclo seguinte de leitura numerada](numbered/README.md#resultado-final) foi
concluído: candidato descartado por aumento agregado de tokens nos pares válidos;
contrato anterior restaurado, suíte completa e refresh aprovados. Os dois
experimentos não produziram uma nova otimização aceita.
