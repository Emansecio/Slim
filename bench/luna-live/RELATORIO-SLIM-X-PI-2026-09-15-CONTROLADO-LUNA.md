# Slim x Pi — comparativo rastreavel (2026-09-15 21:37:02Z)

Modelo: `gpt-5.6-luna`; provider: `openai-codex`; effort high.

## Resumo (todos os bracos com registros, incluindo falhas; uso pode ser parcial)

| Metrica | Slim | Pi | Delta (Slim-Pi) |
|---|---:|---:|---:|
| Chamadas ao modelo | 143 | 173 | -30 |
| Ferramentas executadas | 217 | 232 | -15 |
| Falhas de ferramenta | 2 | 11 | -9 |
| Tokens totais (in+out) | 487483 | 427062 | +60421 |
| Entrada (inclui cache) | 458450 | 398858 | +59592 |
| Cache lido | 158208 | 123904 | +34304 |
| Saida (inclui reasoning) | 29033 | 28204 | +829 |
| Reasoning | 11201 | 9736 | +1465 |
| Arquivos modificados | 24 | 24 | 0 |
| Arquivos criados | 12 | 12 | 0 |
| Tempo wall soma (ms) | 988964 ms | 1297845 ms | -308881 ms |
| Tempo provider soma (ms) | 918662 ms | 1113879 ms | -195217 ms |
| Tempo tools soma (ms) | 61400 ms | 73943 ms | -12543 ms |
| Taxa de acerto de cache | 34.5% | 31.1% | +3.4 pp |

Wall = processo inteiro por braco; provider = soma das latencias informadas pelo harness; tools = soma das duracoes. Residuo = wall-provider-tools: diferenca aritmetica que nao isola startup, pois duracoes de ferramentas podem se sobrepor. Arquivos = diff do workspace vs fixtures originais.

## Pareamento (somente pares com gate aprovado nos dois bracos)

| Medida | Valor |
|---|---:|
| Pares aproveitados | 32 |
| Slim mais economico em tokens | 6/32 |
| Mediana razao tokens Slim/Pi | 1.1457 |
| Mediana razao wall Slim/Pi | 0.9070 |

| Cenario | Pares | Tokens Slim | Tokens Pi | Delta Slim | Razao min/med/max |
|---|---:|---:|---:|---:|---:|
| config_migration | 4 | 75452 | 75025 | +0.57% | 0.45/0.97/1.67 |
| js_pagination | 4 | 47232 | 40244 | +17.36% | 1.14/1.18/1.20 |
| json_cli | 4 | 74099 | 55827 | +32.73% | 1.12/1.40/1.50 |
| ledger_audit | 4 | 83906 | 65843 | +27.43% | 1.12/1.17/1.88 |
| merge_ranges | 4 | 46223 | 33745 | +36.98% | 1.30/1.35/1.48 |
| repair_catalog | 4 | 45176 | 39563 | +14.19% | 1.06/1.15/1.22 |
| repo_wide_repair | 4 | 59469 | 65827 | -9.66% | 0.81/0.91/1.00 |
| sqlite_balances | 4 | 55926 | 50988 | +9.68% | 1.07/1.09/1.13 |

## Por campanha

| Campanha | Cenario | Slim (chamadas/falhas/tokens/wall/arq) | Pi (chamadas/falhas/tokens/wall/arq) |
|---|---|---|---|
| `20260915-205129Z-repair_catalog` | repair_catalog | 4/0/11412/21448ms/1mod+0novos | 5/0/9949/24043ms/1mod+0novos |
| `20260915-205217Z-js_pagination` | js_pagination | 4/0/11602/21241ms/1mod+0novos | 5/0/9988/23969ms/1mod+0novos |
| `20260915-205305Z-json_cli` | json_cli | 5/0/17569/48762ms/0mod+1novos | 5/1/12480/44461ms/0mod+1novos |
| `20260915-205458Z` | merge_ranges | 4/0/11467/19455ms/0mod+1novos | 4/1/8445/21246ms/0mod+1novos |
| `20260915-205540Z-ledger_audit` | ledger_audit | 5/0/19793/24567ms/0mod+1novos | 5/0/10554/23636ms/0mod+1novos |
| `20260915-205630Z-config_migration` | config_migration | 4/0/12062/20375ms/2mod+0novos | 10/1/26576/33399ms/2mod+0novos |
| `20260915-205725Z-js_pagination` | js_pagination | 4/0/11543/21192ms/1mod+0novos | 5/0/9619/25459ms/1mod+0novos |
| `20260915-205816Z-ledger_audit` | ledger_audit | 5/0/19864/21848ms/0mod+1novos | 6/0/17668/24582ms/0mod+1novos |
| `20260915-205903Z-repair_catalog` | repair_catalog | 4/0/11466/19631ms/1mod+0novos | 5/0/9401/19740ms/1mod+0novos |
| `20260915-205945Z-config_migration` | config_migration | 4/0/12273/26336ms/2mod+0novos | 5/1/12329/23941ms/2mod+0novos |
| `20260915-210037Z-json_cli` | json_cli | 5/0/16992/45402ms/0mod+1novos | 5/0/12252/53434ms/0mod+1novos |
| `20260915-210234Z` | merge_ranges | 4/0/11729/21442ms/0mod+1novos | 4/1/7901/23871ms/0mod+1novos |
| `20260915-210320Z-config_migration` | config_migration | 10/2/38972/41096ms/2mod+0novos | 9/1/23368/32425ms/2mod+0novos |
| `20260915-210435Z` | merge_ranges | 4/0/11756/21595ms/0mod+1novos | 4/1/8745/22930ms/0mod+1novos |
| `20260915-210521Z-json_cli` | json_cli | 5/0/18579/60376ms/0mod+1novos | 5/0/12371/45008ms/0mod+1novos |
| `20260915-210724Z-js_pagination` | js_pagination | 4/0/11932/23154ms/1mod+0novos | 5/1/10018/23419ms/1mod+0novos |
| `20260915-210812Z-ledger_audit` | ledger_audit | 5/0/19503/22944ms/0mod+1novos | 6/0/17224/22375ms/0mod+1novos |
| `20260915-210859Z-repair_catalog` | repair_catalog | 4/0/11068/23375ms/1mod+0novos | 5/0/9651/31403ms/1mod+0novos |
| `20260915-210955Z-repair_catalog` | repair_catalog | 4/0/11230/21050ms/1mod+0novos | 6/1/10562/34430ms/1mod+0novos |
| `20260915-211053Z-json_cli` | json_cli | 5/0/20959/67753ms/0mod+1novos | 6/1/18724/59793ms/0mod+1novos |
| `20260915-211318Z-js_pagination` | js_pagination | 4/0/12155/27082ms/1mod+0novos | 5/0/10619/49927ms/1mod+0novos |
| `20260915-211437Z-ledger_audit` | ledger_audit | 6/0/24746/31400ms/0mod+1novos | 7/0/20397/119623ms/0mod+1novos |
| `20260915-211710Z-config_migration` | config_migration | 4/0/12145/23768ms/2mod+0novos | 6/0/12752/51211ms/2mod+0novos |
| `20260915-211826Z` | merge_ranges | 4/0/11271/18627ms/0mod+1novos | 4/1/8654/46335ms/0mod+1novos |
| `20260915-212242Z-sqlite_balances` | sqlite_balances | 4/0/14306/43407ms/1mod+0novos | 5/0/12767/29578ms/1mod+0novos |
| `20260915-212358Z-repo_wide_repair` | repo_wide_repair | 4/0/14900/32849ms/1mod+0novos | 5/0/16347/35622ms/1mod+0novos |
| `20260915-212510Z-sqlite_balances` | sqlite_balances | 4/0/14082/40043ms/1mod+0novos | 5/0/12443/125829ms/1mod+0novos |
| `20260915-212758Z-repo_wide_repair` | repo_wide_repair | 4/0/15060/33994ms/1mod+0novos | 5/0/14986/63307ms/1mod+0novos |
| `20260915-212939Z-sqlite_balances` | sqlite_balances | 4/0/13644/41257ms/1mod+0novos | 5/0/12761/29421ms/1mod+0novos |
| `20260915-213052Z-repo_wide_repair` | repo_wide_repair | 4/0/14665/30596ms/1mod+0novos | 6/0/18007/70552ms/1mod+0novos |
| `20260915-213237Z-repo_wide_repair` | repo_wide_repair | 4/0/14844/37375ms/1mod+0novos | 5/0/16487/36563ms/1mod+0novos |
| `20260915-213353Z-sqlite_balances` | sqlite_balances | 4/0/13894/35524ms/1mod+0novos | 5/0/13017/26313ms/1mod+0novos |

## Por ferramenta (uso agregado por braco)

| Ferramenta | Slim calls/falhas/ms | Pi calls/falhas/ms |
|---|---:|---:|
| read | 128/0/737ms | 118/4/982ms |
| bash | - | 75/7/72839ms |
| shell | 39/2/60372ms | - |
| write | 15/0/40ms | 13/0/26ms |
| edit | - | 26/0/96ms |
| patch | 23/0/163ms | - |
| list | 12/0/88ms | - |

## Erros de ferramenta (para achar fraqueza)

### slim: 2 falha(s) em 217 execucoes

| Classe | Qtd |
|---|---:|
| invalid-json | 2 |

| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |
|---|---|---|---|---|---|---|
| `20260915-210320Z-config_migration` | 3 | shell | `{"command":"python check.py","timeout_ms":120000}` | invalid-json | exit 1 stdout: stderr: Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-75o58b__\slim\check.py", line ... | slim.session.jsonl:call=...7UFwCujk |
| `20260915-210320Z-config_migration` | 6 | shell | `{"command":"python check.py","timeout_ms":120000}` | invalid-json | exit 1 stdout: stderr: Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-75o58b__\slim\check.py", line ... | slim.session.jsonl:call=...ijSTpBuy |

### pi: 11 falha(s) em 232 execucoes

| Classe | Qtd |
|---|---:|
| validation-failed | 4 |
| read-missing | 4 |
| invalid-json | 2 |
| git-command-usage | 1 |

| Campanha | Turno | Tool | Args | Classe | Trecho do erro | Rastro |
|---|---|---|---|---|---|---|
| `20260915-205305Z-json_cli` | 1 | bash | `{"command": "python check.py", "timeout": 120}` | validation-failed | Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-c5_vqnl2\pi\check.py", line 16, in <module> assert re... | pi.audit.jsonl:33f372e197db |
| `20260915-205458Z` | 1 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-962zb8gh\pi\ranges.py' | pi.audit.jsonl:a64922b82dd9 |
| `20260915-205630Z-config_migration` | 5 | bash | `{"command": "python check.py", "timeout": 120}` | invalid-json | Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-a_9e162p\pi\check.py", line 4, in <module> dev = json... | pi.audit.jsonl:107a1b03b2f7 |
| `20260915-205945Z-config_migration` | 4 | bash | `{"command": "git diff -- config/development.json config/production.json config/c...` | git-command-usage | warning: Limiting comparison with pathspecs is only supported if both paths are directories. usage: git diff --no-index [<options>] <path> <... | pi.audit.jsonl:805b8a9c1bbc |
| `20260915-210234Z` | 1 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-lsvr2x_z\pi\ranges.py' | pi.audit.jsonl:81603d4b5518 |
| `20260915-210320Z-config_migration` | 5 | bash | `{"command": "python check.py", "timeout": 10}` | invalid-json | Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-75o58b__\pi\check.py", line 5, in <module> prod = jso... | pi.audit.jsonl:e0171ccead57 |
| `20260915-210435Z` | 1 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-f3r18mza\pi\ranges.py' | pi.audit.jsonl:412b810106b9 |
| `20260915-210724Z-js_pagination` | 2 | bash | `{"command": "python check.py", "timeout": 120}` | validation-failed | Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-khvnsdzy\pi\check.py", line 24, in <module> assert re... | pi.audit.jsonl:1cb10137ce85 |
| `20260915-210955Z-repair_catalog` | 2 | bash | `{"command": "python check.py", "timeout": 120}` | validation-failed | Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-kvemakad\pi\check.py", line 8, in <module> assert vis... | pi.audit.jsonl:af83d945d0dd |
| `20260915-211053Z-json_cli` | 1 | bash | `{"command": "python check.py", "timeout": 120}` | validation-failed | Traceback (most recent call last): File "C:\Users\User\AppData\Local\Temp\slim-pi-luna-f5m_pt8_\pi\check.py", line 16, in <module> assert re... | pi.audit.jsonl:43d9335660f3 |
| `20260915-211826Z` | 1 | read | `{"path": "ranges.py"}` | read-missing | ENOENT: no such file or directory, access 'C:\Users\User\AppData\Local\Temp\slim-pi-luna-t8k2xprf\pi\ranges.py' | pi.audit.jsonl:011f95e3b7e7 |

## Por turno (crescimento de contexto)

### `20260915-205129Z-repair_catalog`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2008 | 127 | 24 | 935 | 5090 | read,read,read,read |
| 2 | 2598 | 368 | 113 | 3198 | 7962 | patch |
| 3 | 3061 | 39 | 12 | 6638 | 3657 | shell |
| 4 | 3138 | 73 | 0 | 8412 | 2743 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1146 | 88 | 14 | 223 | 5075 | read,bash |
| 2 | 1506 | 76 | 11 | 2197 | 2284 | read,read,read |
| 3 | 1947 | 373 | 206 | 4327 | 7611 | edit |
| 4 | 2341 | 35 | 9 | 8045 | 1790 | bash |
| 5 | 2400 | 37 | 0 | 9781 | 1702 |  |

### `20260915-205217Z-js_pagination`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 110 | 30 | 935 | 3177 | read,read,read,read |
| 2 | 2713 | 329 | 69 | 3164 | 7348 | patch |
| 3 | 3140 | 42 | 11 | 6130 | 2118 | shell |
| 4 | 3219 | 38 | 0 | 7918 | 6579 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1147 | 29 | 9 | 226 | 2883 | read |
| 2 | 1293 | 65 | 0 | 1931 | 3062 | read,read,bash |
| 3 | 1957 | 477 | 237 | 2470 | 9662 | edit |
| 4 | 2456 | 24 | 0 | 6462 | 1454 | bash |
| 5 | 2503 | 37 | 0 | 6666 | 2252 |  |

### `20260915-205305Z-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2016 | 108 | 24 | 960 | 3574 | read,read,list,list |
| 2 | 2699 | 50 | 25 | 3181 | 2419 | read |
| 3 | 2841 | 1198 | 516 | 5031 | 22837 | write |
| 4 | 4084 | 37 | 10 | 13230 | 1937 | shell |
| 5 | 4160 | 376 | 314 | 14976 | 9432 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1151 | 97 | 12 | 241 | 3088 | read,bash,bash |
| 2 | 1701 | 35 | 15 | 2394 | 1880 | read |
| 3 | 2139 | 963 | 298 | 4175 | 18662 | write |
| 4 | 3118 | 33 | 7 | 10639 | 2400 | bash |
| 5 | 3176 | 67 | 0 | 12290 | 3070 |  |

### `20260915-205458Z`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2006 | 87 | 21 | 924 | 3869 | read,read,list |
| 2 | 2555 | 497 | 227 | 2981 | 10128 | write |
| 3 | 3094 | 25 | 0 | 7117 | 1865 | shell |
| 4 | 3163 | 40 | 0 | 7333 | 2804 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1151 | 92 | 12 | 245 | 4173 | read,read,read,bash |
| 2 | 1843 | 504 | 138 | 2515 | 9927 | write |
| 3 | 2362 | 24 | 0 | 6566 | 1386 | bash |
| 4 | 2416 | 53 | 0 | 6771 | 1785 |  |

### `20260915-205540Z-ledger_audit`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 105 | 23 | 939 | 4002 | read,read,read |
| 2 | 3838 | 390 | 233 | 3046 | 9750 | shell |
| 3 | 4306 | 106 | 22 | 6739 | 4721 | write |
| 4 | 4453 | 29 | 0 | 8762 | 2090 | shell |
| 5 | 4524 | 31 | 0 | 8993 | 2686 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1154 | 87 | 21 | 245 | 3364 | bash,read,read |
| 2 | 1846 | 285 | 107 | 2378 | 7742 | bash |
| 3 | 2197 | 157 | 73 | 5264 | 4772 | write |
| 4 | 2369 | 20 | 0 | 7637 | 1917 | bash |
| 5 | 2417 | 22 | 0 | 7826 | 1223 |  |

### `20260915-205630Z-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2017 | 122 | 25 | 992 | 5209 | read,read,read,read,read |
| 2 | 2737 | 408 | 108 | 3329 | 9002 | patch,patch |
| 3 | 3291 | 52 | 21 | 6890 | 2399 | shell |
| 4 | 3387 | 48 | 0 | 8735 | 3017 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1146 | 88 | 14 | 222 | 3872 | read,bash |
| 2 | 1499 | 116 | 33 | 2198 | 3076 | read,read,read,read |
| 3 | 2104 | 255 | 117 | 4635 | 5548 | edit |
| 4 | 2382 | 110 | 0 | 7544 | 3267 | edit |
| 5 | 2515 | 36 | 10 | 8056 | 1778 | bash |
| 6 | 2839 | 96 | 42 | 9791 | 2595 | read,read |
| 7 | 3149 | 89 | 27 | 11956 | 2675 | edit |
| 8 | 3261 | 61 | 0 | 13987 | 2156 | edit |
| 9 | 3345 | 33 | 7 | 14333 | 1644 | bash |
| 10 | 3408 | 44 | 0 | 15984 | 2206 |  |

### `20260915-205725Z-js_pagination`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 130 | 26 | 935 | 7962 | read,read,read,read |
| 2 | 2733 | 288 | 40 | 3206 | 7164 | patch |
| 3 | 3119 | 36 | 9 | 5923 | 2090 | shell |
| 4 | 3192 | 34 | 0 | 7677 | 3016 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1149 | 92 | 8 | 226 | 4436 | bash,read,bash |
| 2 | 1408 | 64 | 0 | 2340 | 3135 | read,read,read |
| 3 | 1942 | 310 | 57 | 2881 | 8350 | edit |
| 4 | 2274 | 24 | 0 | 5729 | 2896 | bash |
| 5 | 2321 | 35 | 0 | 5933 | 2433 |  |

### `20260915-205816Z-ledger_audit`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 104 | 22 | 939 | 3102 | read,read,read |
| 2 | 3837 | 410 | 242 | 3034 | 8864 | shell |
| 3 | 4325 | 98 | 14 | 6763 | 3563 | write |
| 4 | 4464 | 41 | 10 | 8705 | 3382 | shell |
| 5 | 4547 | 27 | 0 | 10458 | 1590 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1153 | 88 | 16 | 245 | 3427 | bash,read |
| 2 | 1511 | 61 | 11 | 2234 | 2061 | read,read |
| 3 | 3184 | 364 | 217 | 4140 | 7488 | bash |
| 4 | 3603 | 115 | 31 | 7590 | 3685 | write |
| 5 | 3733 | 36 | 10 | 9679 | 1751 | bash |
| 6 | 3797 | 23 | 0 | 11413 | 1646 |  |

### `20260915-205903Z-repair_catalog`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2008 | 121 | 18 | 935 | 3704 | read,read,read,read |
| 2 | 2592 | 399 | 209 | 3154 | 9597 | patch |
| 3 | 3086 | 37 | 10 | 7058 | 2312 | shell |
| 4 | 3161 | 62 | 0 | 8800 | 2899 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1146 | 73 | 8 | 223 | 2881 | read,bash |
| 2 | 1491 | 63 | 0 | 2084 | 3526 | read,read,read |
| 3 | 1919 | 231 | 62 | 2628 | 5057 | edit |
| 4 | 2171 | 34 | 8 | 5349 | 2079 | bash |
| 5 | 2229 | 44 | 0 | 7089 | 1801 |  |

### `20260915-205945Z-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2017 | 116 | 19 | 992 | 3998 | read,read,read,read,read |
| 2 | 2731 | 611 | 115 | 3274 | 16226 | write,write |
| 3 | 3303 | 50 | 23 | 7544 | 3047 | shell |
| 4 | 3397 | 48 | 0 | 9368 | 2117 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1145 | 99 | 25 | 222 | 3087 | read,bash |
| 2 | 1509 | 91 | 8 | 2256 | 2493 | read,read,read,read |
| 3 | 2089 | 364 | 102 | 4540 | 7874 | edit,edit |
| 4 | 2497 | 91 | 18 | 7757 | 2694 | bash,bash |
| 5 | 4303 | 141 | 101 | 9842 | 3381 |  |

### `20260915-210037Z-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2016 | 106 | 28 | 960 | 3089 | read,read,list |
| 2 | 2682 | 48 | 17 | 3108 | 2267 | read |
| 3 | 2822 | 1018 | 264 | 4927 | 19643 | write |
| 4 | 3886 | 41 | 10 | 11664 | 3146 | shell |
| 5 | 3966 | 407 | 341 | 13425 | 8843 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1154 | 32 | 12 | 241 | 4207 | read |
| 2 | 1323 | 82 | 8 | 1972 | 4363 | bash,read |
| 3 | 1963 | 1195 | 354 | 3917 | 24563 | write |
| 4 | 3174 | 36 | 10 | 11638 | 4028 | bash |
| 5 | 3235 | 58 | 0 | 13372 | 4577 |  |

### `20260915-210234Z`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2006 | 101 | 23 | 924 | 4583 | read,read,list |
| 2 | 2569 | 559 | 258 | 3039 | 11746 | write |
| 3 | 3171 | 36 | 9 | 7676 | 2187 | shell |
| 4 | 3251 | 36 | 0 | 9422 | 2186 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1152 | 72 | 8 | 245 | 5257 | read,read,read |
| 2 | 1714 | 458 | 117 | 2248 | 9349 | write |
| 3 | 2187 | 24 | 0 | 6042 | 1458 | bash |
| 4 | 2241 | 53 | 0 | 6247 | 3869 |  |

### `20260915-210320Z-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2017 | 150 | 23 | 992 | 4060 | read,read,read,read,read |
| 2 | 2765 | 380 | 129 | 3427 | 8491 | patch,patch |
| 3 | 3292 | 39 | 12 | 6945 | 2360 | shell |
| 4 | 3626 | 91 | 63 | 8711 | 5375 | read |
| 5 | 3850 | 100 | 45 | 10882 | 2889 | patch |
| 6 | 4049 | 34 | 7 | 13030 | 2831 | shell |
| 7 | 4378 | 50 | 22 | 14692 | 1998 | read |
| 8 | 4525 | 63 | 7 | 16557 | 4523 | patch |
| 9 | 4688 | 35 | 8 | 18400 | 2877 | shell |
| 10 | 4767 | 73 | 23 | 20082 | 3460 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1146 | 109 | 18 | 222 | 3520 | read,bash |
| 2 | 1408 | 188 | 13 | 2258 | 4405 | read,read,read,read |
| 3 | 2085 | 242 | 80 | 4825 | 5315 | edit |
| 4 | 2373 | 133 | 0 | 7577 | 3383 | edit |
| 5 | 2552 | 33 | 7 | 8147 | 1665 | bash |
| 6 | 2873 | 120 | 20 | 9797 | 3240 | read,read |
| 7 | 3207 | 95 | 10 | 11918 | 3443 | edit |
| 8 | 3348 | 24 | 0 | 13847 | 1457 | bash |
| 9 | 3402 | 30 | 0 | 14051 | 1458 |  |

### `20260915-210435Z`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2006 | 101 | 23 | 924 | 3372 | read,read,list |
| 2 | 2569 | 555 | 238 | 3046 | 11294 | write |
| 3 | 3169 | 29 | 0 | 7539 | 3008 | shell |
| 4 | 3242 | 85 | 47 | 7770 | 3169 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1152 | 92 | 12 | 245 | 3219 | read,read,read,bash |
| 2 | 1845 | 600 | 235 | 2515 | 12112 | write |
| 3 | 2460 | 34 | 8 | 7298 | 1775 | bash |
| 4 | 2524 | 38 | 0 | 8969 | 1851 |  |

### `20260915-210521Z-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2016 | 111 | 33 | 960 | 3770 | read,read,list |
| 2 | 2687 | 46 | 15 | 3152 | 2780 | read |
| 3 | 2825 | 1419 | 686 | 4960 | 27090 | write |
| 4 | 4289 | 40 | 9 | 16004 | 2342 | shell |
| 5 | 4368 | 778 | 717 | 17763 | 15974 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1151 | 82 | 8 | 241 | 3196 | read,bash |
| 2 | 1525 | 34 | 14 | 2124 | 2458 | read |
| 3 | 1962 | 1171 | 457 | 3888 | 22481 | write |
| 4 | 3149 | 33 | 7 | 11942 | 2040 | bash |
| 5 | 3207 | 57 | 0 | 13592 | 2203 |  |

### `20260915-210724Z-js_pagination`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 131 | 27 | 935 | 4304 | read,read,read,read |
| 2 | 2734 | 416 | 163 | 3206 | 11968 | patch |
| 3 | 3248 | 37 | 10 | 6808 | 2005 | shell |
| 4 | 3322 | 33 | 0 | 8550 | 3907 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1148 | 34 | 14 | 226 | 2292 | read |
| 2 | 1299 | 64 | 7 | 1962 | 2027 | read,bash |
| 3 | 1846 | 596 | 336 | 3793 | 11492 | edit |
| 4 | 2464 | 24 | 0 | 8236 | 1303 | bash |
| 5 | 2511 | 32 | 0 | 8441 | 1451 |  |

### `20260915-210812Z-ledger_audit`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 101 | 19 | 939 | 3595 | read,read,read |
| 2 | 3834 | 322 | 154 | 2999 | 8385 | shell |
| 3 | 4234 | 107 | 23 | 6332 | 3093 | write |
| 4 | 4382 | 29 | 0 | 8374 | 4593 | shell |
| 5 | 4453 | 30 | 0 | 8604 | 1884 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1154 | 72 | 22 | 245 | 2845 | bash |
| 2 | 1382 | 62 | 0 | 2128 | 2108 | read,read,read |
| 3 | 3159 | 312 | 164 | 2666 | 6607 | bash |
| 4 | 3517 | 133 | 49 | 5761 | 3289 | write |
| 5 | 3665 | 24 | 0 | 7984 | 1428 | bash |
| 6 | 3717 | 27 | 0 | 8189 | 1512 |  |

### `20260915-210859Z-repair_catalog`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2008 | 122 | 19 | 935 | 3519 | read,read,read,read |
| 2 | 2593 | 272 | 112 | 3149 | 8719 | patch |
| 3 | 2964 | 37 | 10 | 6170 | 6257 | shell |
| 4 | 3039 | 33 | 0 | 7916 | 4108 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1148 | 67 | 13 | 223 | 4153 | bash |
| 2 | 1399 | 77 | 0 | 2048 | 3798 | read,read,read,read |
| 3 | 1932 | 337 | 171 | 2770 | 9420 | edit |
| 4 | 2290 | 24 | 0 | 6299 | 4727 | bash |
| 5 | 2338 | 39 | 0 | 6504 | 4666 |  |

### `20260915-210955Z-repair_catalog`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2008 | 125 | 22 | 935 | 3589 | read,read,read,read |
| 2 | 2596 | 328 | 72 | 3198 | 8192 | patch |
| 3 | 3018 | 37 | 10 | 6404 | 2092 | shell |
| 4 | 3093 | 25 | 0 | 8150 | 6217 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1144 | 28 | 8 | 223 | 5446 | read |
| 2 | 1265 | 69 | 0 | 1934 | 5709 | read,read,bash |
| 3 | 1554 | 38 | 18 | 2504 | 4259 | read |
| 4 | 1849 | 253 | 80 | 4278 | 7990 | edit |
| 5 | 2123 | 24 | 0 | 7084 | 3156 | bash |
| 6 | 2171 | 44 | 0 | 7289 | 3191 |  |

### `20260915-211053Z-json_cli`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2016 | 105 | 21 | 960 | 3219 | read,read,list,list |
| 2 | 2696 | 93 | 17 | 3137 | 2812 | read,shell |
| 3 | 3036 | 2126 | 1313 | 5139 | 41226 | write |
| 4 | 5209 | 37 | 10 | 22311 | 3723 | shell |
| 5 | 5285 | 356 | 288 | 24061 | 7911 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1151 | 102 | 8 | 241 | 3329 | read,bash,bash |
| 2 | 1705 | 35 | 15 | 2394 | 1759 | read |
| 3 | 2143 | 1503 | 623 | 4155 | 27972 | write |
| 4 | 3662 | 33 | 7 | 15687 | 1667 | bash |
| 5 | 3720 | 351 | 326 | 17338 | 8128 | read |
| 6 | 4156 | 163 | 96 | 21433 | 3980 |  |

### `20260915-211318Z-js_pagination`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 131 | 27 | 935 | 3831 | read,read,read,read |
| 2 | 2734 | 490 | 252 | 3215 | 12473 | patch |
| 3 | 3320 | 39 | 12 | 7331 | 2011 | shell |
| 4 | 3396 | 34 | 0 | 9101 | 7774 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1147 | 88 | 14 | 226 | 7946 | read,bash |
| 2 | 1537 | 83 | 17 | 2204 | 10446 | read,read,read |
| 3 | 2090 | 447 | 189 | 4339 | 14120 | edit |
| 4 | 2559 | 24 | 0 | 7998 | 6607 | bash |
| 5 | 2606 | 38 | 0 | 8202 | 6615 |  |

### `20260915-211437Z-ledger_audit`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2011 | 109 | 27 | 939 | 3055 | read,read,read |
| 2 | 3842 | 395 | 229 | 3010 | 8629 | shell |
| 3 | 4315 | 137 | 115 | 6758 | 3826 | list |
| 4 | 4477 | 94 | 10 | 9254 | 3543 | write |
| 5 | 4612 | 37 | 10 | 11171 | 3764 | shell |
| 6 | 4691 | 26 | 0 | 12917 | 7061 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1153 | 91 | 17 | 245 | 7220 | read,bash |
| 2 | 1500 | 30 | 10 | 2238 | 7177 | read |
| 3 | 2781 | 149 | 129 | 3948 | 9192 | read |
| 4 | 3293 | 191 | 49 | 6417 | 9948 | bash |
| 5 | 3526 | 160 | 76 | 8812 | 67539 | write |
| 6 | 3701 | 36 | 10 | 11210 | 6400 | bash |
| 7 | 3765 | 21 | 0 | 12941 | 7560 |  |

### `20260915-211710Z-config_migration`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2017 | 156 | 29 | 992 | 10029 | read,read,read,read,read |
| 2 | 2771 | 395 | 143 | 3447 | 8985 | patch,patch |
| 3 | 3312 | 48 | 21 | 7084 | 2098 | shell |
| 4 | 3404 | 42 | 0 | 8914 | 1837 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1146 | 86 | 12 | 222 | 7426 | read,bash |
| 2 | 1497 | 100 | 17 | 2186 | 7412 | read,read,read,read |
| 3 | 2086 | 248 | 109 | 4540 | 11300 | edit |
| 4 | 2357 | 111 | 0 | 7374 | 7531 | edit |
| 5 | 2491 | 44 | 18 | 7887 | 7313 | bash |
| 6 | 2565 | 21 | 0 | 9701 | 6147 |  |

### `20260915-211826Z`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2006 | 101 | 23 | 924 | 2992 | read,read,list |
| 2 | 2569 | 415 | 135 | 3036 | 8450 | write |
| 3 | 3025 | 25 | 0 | 6628 | 4650 | shell |
| 4 | 3094 | 36 | 0 | 6844 | 1699 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1153 | 86 | 8 | 245 | 7485 | read,read,read,bash |
| 2 | 1745 | 683 | 340 | 2489 | 21274 | write |
| 3 | 2443 | 24 | 0 | 7870 | 6887 | bash |
| 4 | 2497 | 23 | 0 | 8075 | 6657 |  |

### `20260915-212242Z-sqlite_balances`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2015 | 129 | 24 | 964 | 11568 | read,read,read,read |
| 2 | 3158 | 780 | 343 | 3230 | 15643 | patch |
| 3 | 4038 | 35 | 8 | 8362 | 5913 | shell |
| 4 | 4117 | 34 | 0 | 10101 | 8270 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1147 | 83 | 18 | 223 | 3605 | bash,read |
| 2 | 1589 | 80 | 13 | 2201 | 2917 | read,read,read |
| 3 | 2502 | 737 | 358 | 4338 | 14631 | write |
| 4 | 3257 | 24 | 0 | 9528 | 1525 | bash |
| 5 | 3311 | 37 | 0 | 9733 | 1937 |  |

### `20260915-212358Z-repo_wide_repair`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2091 | 197 | 20 | 1321 | 5875 | read,read,read,read,read,read,read |
| 2 | 3398 | 674 | 213 | 3992 | 13467 | patch |
| 3 | 4177 | 37 | 10 | 8762 | 3452 | shell |
| 4 | 4256 | 70 | 0 | 10508 | 9056 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1150 | 124 | 18 | 245 | 3649 | read,bash |
| 2 | 2204 | 185 | 15 | 2353 | 4441 | read,read,read,bash |
| 3 | 3283 | 870 | 357 | 4914 | 17108 | edit |
| 4 | 4202 | 36 | 10 | 10672 | 2482 | bash |
| 5 | 4266 | 27 | 0 | 12406 | 1840 |  |

### `20260915-212510Z-sqlite_balances`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2015 | 114 | 33 | 964 | 8215 | read,read,read,read |
| 2 | 3143 | 722 | 356 | 3215 | 20837 | patch |
| 3 | 3965 | 35 | 8 | 8176 | 2242 | shell |
| 4 | 4044 | 44 | 0 | 9917 | 7775 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1148 | 105 | 31 | 223 | 3309 | read,bash |
| 2 | 1612 | 84 | 17 | 2312 | 2387 | read,read,read |
| 3 | 2529 | 574 | 163 | 4461 | 94982 | edit |
| 4 | 3127 | 35 | 9 | 8317 | 8541 | bash |
| 5 | 3192 | 37 | 0 | 10053 | 10785 |  |

### `20260915-212758Z-repo_wide_repair`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2091 | 199 | 22 | 1321 | 4774 | read,read,read,read,read,read,read |
| 2 | 3400 | 728 | 267 | 4015 | 18508 | patch |
| 3 | 4233 | 37 | 10 | 9166 | 2050 | shell |
| 4 | 4312 | 60 | 0 | 10911 | 6660 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1149 | 59 | 30 | 245 | 8281 | read |
| 2 | 1403 | 118 | 8 | 2106 | 8605 | bash,read,read |
| 3 | 3098 | 922 | 484 | 4305 | 26134 | edit |
| 4 | 4045 | 36 | 10 | 10821 | 7743 | bash |
| 5 | 4109 | 47 | 0 | 12556 | 7676 |  |

### `20260915-212939Z-sqlite_balances`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2015 | 130 | 25 | 964 | 8506 | read,read,read,read |
| 2 | 3159 | 552 | 159 | 3251 | 26857 | patch |
| 3 | 3812 | 40 | 9 | 7075 | 2203 | shell |
| 4 | 3896 | 40 | 0 | 8840 | 1771 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1149 | 96 | 22 | 223 | 3788 | read,bash |
| 2 | 1604 | 65 | 0 | 2247 | 2387 | read,read,read |
| 3 | 2502 | 728 | 321 | 2807 | 14197 | edit |
| 4 | 3254 | 24 | 0 | 7643 | 2315 | bash |
| 5 | 3308 | 31 | 0 | 7848 | 1814 |  |

### `20260915-213052Z-repo_wide_repair`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2091 | 168 | 20 | 1321 | 11346 | read,read,read,read,read,list |
| 2 | 3376 | 631 | 174 | 3842 | 13740 | patch |
| 3 | 4109 | 41 | 10 | 8275 | 1703 | shell |
| 4 | 4192 | 57 | 0 | 10036 | 2893 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1148 | 36 | 8 | 245 | 7684 | read |
| 2 | 1379 | 52 | 8 | 1981 | 6711 | bash |
| 3 | 2199 | 142 | 0 | 3747 | 9382 | read,read,read,read,read |
| 4 | 3247 | 1040 | 506 | 4820 | 25601 | edit |
| 5 | 4312 | 35 | 9 | 11660 | 8543 | bash |
| 6 | 4375 | 42 | 0 | 13395 | 6851 |  |

### `20260915-213237Z-repo_wide_repair`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2091 | 198 | 21 | 1321 | 5208 | read,read,read,read,read,read,read |
| 2 | 3399 | 654 | 195 | 4025 | 18463 | patch |
| 3 | 4156 | 43 | 16 | 8731 | 8028 | shell |
| 4 | 4241 | 62 | 0 | 10525 | 2509 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1148 | 110 | 36 | 245 | 3695 | read,bash |
| 2 | 2196 | 106 | 12 | 2375 | 2923 | read,read,read,bash |
| 3 | 3196 | 1050 | 516 | 4689 | 19972 | edit |
| 4 | 4271 | 36 | 10 | 11490 | 2124 | bash |
| 5 | 4335 | 39 | 0 | 13222 | 2220 |  |

### `20260915-213353Z-sqlite_balances`

#### slim

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 2015 | 131 | 26 | 964 | 8609 | read,read,read,read |
| 2 | 3160 | 718 | 259 | 3266 | 14568 | write |
| 3 | 3852 | 39 | 8 | 8002 | 3124 | shell |
| 4 | 3935 | 44 | 0 | 9768 | 8403 |  |

#### pi

| turno | in | out | reasoning | hist_bytes | provider_ms | tools |
|---|---:|---:|---:|---:|---:|---|
| 1 | 1150 | 92 | 12 | 223 | 3592 | read,read,read,bash |
| 2 | 2310 | 31 | 8 | 2488 | 1984 | read |
| 3 | 2446 | 646 | 256 | 4154 | 12942 | edit |
| 4 | 3116 | 24 | 0 | 8581 | 1789 | bash |
| 5 | 3170 | 32 | 0 | 8786 | 1827 |  |

## Rastreabilidade

### `20260915-205129Z-repair_catalog` — repair_catalog (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205217Z-js_pagination` — js_pagination (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205305Z-json_cli` — json_cli (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205458Z` — merge_ranges (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205540Z-ledger_audit` — ledger_audit (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205630Z-config_migration` — config_migration (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205725Z-js_pagination` — js_pagination (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205816Z-ledger_audit` — ledger_audit (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205903Z-repair_catalog` — repair_catalog (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-205945Z-config_migration` — config_migration (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210037Z-json_cli` — json_cli (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210234Z` — merge_ranges (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210320Z-config_migration` — config_migration (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210435Z` — merge_ranges (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210521Z-json_cli` — json_cli (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210724Z-js_pagination` — js_pagination (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210812Z-ledger_audit` — ledger_audit (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210859Z-repair_catalog` — repair_catalog (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-210955Z-repair_catalog` — repair_catalog (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-211053Z-json_cli` — json_cli (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-211318Z-js_pagination` — js_pagination (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-211437Z-ledger_audit` — ledger_audit (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-211710Z-config_migration` — config_migration (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-211826Z` — merge_ranges (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-212242Z-sqlite_balances` — sqlite_balances (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-212358Z-repo_wide_repair` — repo_wide_repair (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-212510Z-sqlite_balances` — sqlite_balances (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-212758Z-repo_wide_repair` — repo_wide_repair (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-212939Z-sqlite_balances` — sqlite_balances (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213052Z-repo_wide_repair` — repo_wide_repair (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213237Z-repo_wide_repair` — repo_wide_repair (ordem: ["pi", "slim"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

### `20260915-213353Z-sqlite_balances` — sqlite_balances (ordem: ["slim", "pi"])

- slim: `slim 0.1.0` sha256 `5e04cb23c16cf0fadb585c2d2dc517fa7dee91a1ecba5dc130207fd67f15f925`
- pi: `0.85.1` sha256 `e6d7fcf36a239cf3746e67ddf4222081ac01a601b85a3ee688bdfe9c161d754c`
- arquivos: `manifest.json`, `prompt.txt`, `SPEC.md`, `check.py`, `pi.audit.jsonl`, `pi.session.jsonl`, `slim.session.jsonl`, `slim.stdout.jsonl`, `*.timing.json`, `*.validation.json`, `*.workspace/`.
- validacao externa: `check.py`/`SPEC.md` originais executados fora do workspace do modelo; `fixtures_unchanged` + exit 0 exigidos.

## Reproducao

```powershell
python bench/luna-live/daily.py --rounds 1
python bench/luna-live/report.py <campanhas...> --output-md RELATORIO.md --output-json report.json
```

## Limitacoes

- Amostra pequena, host nao exclusivo, caches do servidor nao controlados, ordem alternada mas sem randomizacao plena.
- Instrumentacao assimetrica: Pi via extensao observadora, Slim via ledger/sessao; contagens sao turnos do modelo, nao TCP/retries de transporte. Residuo nao isola startup: duracoes de tools podem se sobrepor.
- history_bytes exclui resultados de tools nos dois bracos; tool_result_bytes os registra separadamente. Campanhas Pi antigas reconstroem bytes do payload com reasoning opaco omitido, portanto seus bytes de historico sao parciais.
- Tokens sao usos informados pelos providers; sem inferencia de custo monetario.
- Outcomes Slim: facts estruturados tool.v1 quando presentes; senao heuristica sobre o texto da tool.
- Todos os bracos passaram no gate (exit 0, validacao externa PASS, fixtures intactas, metricas completas).
