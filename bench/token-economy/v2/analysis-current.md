# Analysis — campaign `fixed-20260830`

## Gate

**PASS** — 36/36 arms; 108/108 compliant requests.

Payload values below are UTF-8 bytes in the captured JSON request. `~tokens` remains only bytes ÷ 4 and is not provider billing usage.

## Compliance

| agent | model | effort | requests | ok |
|---|---|---|---:|---|
| slim | gpt-5.6-luna | high | 36/36 | PASS |
| pi | gpt-5.6-luna | high | 36/36 | PASS |
| pit | gpt-5.6-luna | high | 36/36 | PASS |

## Payload (medians across runs)

### s1_read

| agent | requests/run | T1 total | T1 system content | T1 tool schemas (n) | Tn total | Tn history | sum all requests | growth | ~tokens Tn |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| slim | 2 | 7259 B | 2557 B | 4491 B (9) | 10137 B | 2953 B | 17396 B | ×1.4 | ~2534 |
| pi | 2 | 5985 B | 2782 B | 2901 B (4) | 8773 B | 2888 B | 14758 B | ×1.47 | ~2193 |
| pit | 2 | 11914 B | 4029 B | 7434 B (12) | 14702 B | 3067 B | 26616 B | ×1.23 | ~3675 |

### s2_codegen

| agent | requests/run | T1 total | T1 system content | T1 tool schemas (n) | Tn total | Tn history | sum all requests | growth | ~tokens Tn |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| slim | 2 | 7260 B | 2557 B | 4491 B (9) | 7941 B | 757 B | 15201 B | ×1.09 | ~1985 |
| pi | 2 | 5989 B | 2785 B | 2901 B (4) | 6690 B | 802 B | 12679 B | ×1.12 | ~1672 |
| pit | 2 | 12144 B | 4029 B | 7434 B (12) | 12495 B | 860 B | 24639 B | ×1.03 | ~3123 |

### s3_multistep

| agent | requests/run | T1 total | T1 system content | T1 tool schemas (n) | Tn total | Tn history | sum all requests | growth | ~tokens Tn |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| slim | 3 | 7260 B | 2557 B | 4491 B (9) | 8656 B | 1470 B | 23857 B | ×1.19 | ~2164 |
| pi | 3 | 5991 B | 2787 B | 2901 B (4) | 7329 B | 1437 B | 20012 B | ×1.22 | ~1832 |
| pit | 3 | 12146 B | 4029 B | 7434 B (12) | 13134 B | 1497 B | 37777 B | ×1.08 | ~3283 |

### s4_long

| agent | requests/run | T1 total | T1 system content | T1 tool schemas (n) | Tn total | Tn history | sum all requests | growth | ~tokens Tn |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| slim | 5 | 7260 B | 2557 B | 4491 B (9) | 11858 B | 4668 B | 50861 B | ×1.63 | ~2964 |
| pi | 5 | 5986 B | 2782 B | 2901 B (4) | 12900 B | 7009 B | 51485 B | ×2.15 | ~3225 |
| pit | 5 | 12141 B | 4029 B | 7434 B (12) | 16353 B | 4712 B | 74504 B | ×1.35 | ~4088 |

## Speed (direct medians/minima)

### s1_read

| agent | startup median/min | post-write request median/min | turn gap median | process median/min |
|---|---:|---:|---:|---:|
| slim | 15/15 ms | —/— ms | 3 ms | 23/23 ms |
| pi | 324/315 ms | —/— ms | 19 ms | 357/349 ms |
| pit | 3171/2936 ms | —/— ms | 74 ms | 3358/3068 ms |

### s2_codegen

| agent | startup median/min | post-write request median/min | turn gap median | process median/min |
|---|---:|---:|---:|---:|
| slim | 17/15 ms | 24/20 ms | 6 ms | 28/26 ms |
| pi | 318/314 ms | 339/332 ms | 18 ms | 355/349 ms |
| pit | 2938/2893 ms | 3037/2997 ms | 104 ms | 3166/3102 ms |

### s3_multistep

| agent | startup median/min | post-write request median/min | turn gap median | process median/min |
|---|---:|---:|---:|---:|
| slim | 15/15 ms | 21/21 ms | 4 ms | 28/27 ms |
| pi | 324/320 ms | 345/340 ms | 14 ms | 409/406 ms |
| pit | 2952/2900 ms | 3052/2996 ms | 56 ms | 3188/3099 ms |

### s4_long

| agent | startup median/min | post-write request median/min | turn gap median | process median/min |
|---|---:|---:|---:|---:|
| slim | 17/14 ms | 28/26 ms | 3 ms | 37/35 ms |
| pi | 326/315 ms | 360/349 ms | 8 ms | 419/400 ms |
| pit | 2972/2969 ms | 3067/3065 ms | 24 ms | 3154/3150 ms |

## Definitions

- startup = first request arrival epoch − process start epoch.
- post-write request = first request sent after the write tool returns − process start epoch.
- process total is measured around the agent subprocess only; no PowerShell `Start-Job`.
- all individual run samples are retained in `summary_v2.json` and raw requests in `captures/`.
