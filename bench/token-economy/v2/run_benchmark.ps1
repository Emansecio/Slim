# Requires -Version 5
<#
.SYNOPSIS
    Token-economy + speed benchmark v2: Slim x Pi x Pit, matrix of scenarios x runs.

.DESCRIPTION
    Assumes capture_server.py (v2) is already running on 127.0.0.1:8931.
    For each scenario (s1_read, s2_codegen, s3_multistep, s4_long) and each
    run (default 3), runs the SAME scripted task on every agent and records:

      - request bodies   -> captures\<scenario>\run<N>\<agent>\req_<n>.json
      - server timings   -> captures\<scenario>\run<N>\<agent>\req_<n>.meta.json
      - process timings  -> runs\<scenario>_run<N>_<agent>_timing.json

    Model pinned for all agents: gpt-5.6-luna with reasoning effort HIGH.
    Pi/Pit run against isolated agent dirs (.agent-dirs-v2); the user's real
    configuration is never touched.

    Slim headless consumes SLIM_EFFORT/layered config and sends
    reasoning_effort on the wire. The runner exports SLIM_EFFORT=high and the
    compliance gate rejects any regression. See PLAN.md "Pinagem do modelo".
#>
param(
    [string]$SlimPath = "$env:USERPROFILE\bin\Slim.exe",
    [int]$Port = 8931,
    [int]$Runs = 3,
    [string[]]$Scenarios = @('s1_read', 's2_codegen', 's3_multistep', 's4_long'),
    [string[]]$Agents = @('slim', 'pi', 'pit'),
    # TOK-08 A/B: when $ExtraPrompt is set, it is appended to every task
    # prompt and $VariantTag suffices the capture tag, so results appear as
    # separate agent rows (e.g. slim_econ) comparable against the baseline.
    [string]$VariantTag = '',
    [string]$ExtraPrompt = ''
)
$ErrorActionPreference = 'Continue'
$bench = $PSScriptRoot
$captures = Join-Path $bench 'captures'
$runsDir = Join-Path $bench 'runs'
$envRoot = Join-Path $bench '.agent-dirs-v2'
New-Item -ItemType Directory -Force -Path $captures, $runsDir, $envRoot | Out-Null

# ---- fixed task -------------------------------------------------------------
$taskDir = Join-Path $bench '.task-v2'
New-Item -ItemType Directory -Force -Path $taskDir | Out-Null
Remove-Item (Join-Path $taskDir '*') -Recurse -Force -ErrorAction SilentlyContinue
$lines = 1..40 | ForEach-Object { "line-$_ : the quick brown fox jumps over the lazy dog 0x{0:x4}" -f $_ }
Set-Content -Path (Join-Path $taskDir 'bench-target.txt') -Value ($lines -join "`r`n")
$prompt = 'Read bench-target.txt and tell me its first line.'
$codegenPrompt = 'Create fizzbuzz.py implementing fizzbuzz up to 25.'

$model = 'gpt-5.6-luna'
$thinking = 'high'

# ---- models.json for pi / pit (pinned model + thinking map) -----------------
$models = @{
    providers = @{
        tokenbench = @{
            baseUrl = "http://127.0.0.1:$Port/v1"
            api     = 'openai-completions'
            apiKey  = 'bench-key'
            models  = @(@{
                id         = $model
                reasoning  = $true
                thinkingLevelMap = @{ high = $thinking }
                compat     = @{ supportsDeveloperRole = $false }
            })
        }
    }
} | ConvertTo-Json -Depth 8

foreach ($name in @('pi', 'pit')) {
    $dir = Join-Path $envRoot "$name-agent"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Set-Content -Path (Join-Path $dir 'models.json') -Value $models
}

function Reset-Capture([string]$scenario, [string]$tag, [string]$run) {
    Invoke-RestMethod -Method Post -ContentType "application/json" `
        -Uri ("http://127.0.0.1:$Port/reset" + "?scenario=$scenario&tag=$tag&run=$run") | Out-Null
}

function Save-Captures([string]$agent, [string]$scenario, [string]$run) {
    # Server now writes directly into captures/<scenario>/run<run>/<agent>/;
    # nothing to move. Kept as no-op for compatibility.
}

function Run-Agent([string]$name, [string]$scenario, [string]$run, [scriptblock]$invocation) {
    Write-Host "=== $scenario / run $run / $name ==="
    Reset-Capture $scenario "$name" "$run"
    $t = @{
        scenario = $scenario; run = $run; name = $name;
        startEpochMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $job = Start-Job -ScriptBlock $invocation
    if (Wait-Job $job -Timeout 240) { Receive-Job $job | Out-Host }
    else { Write-Warning "$name timed out"; Stop-Job $job }
    Remove-Job $job -Force -ErrorAction SilentlyContinue
    $sw.Stop()
    $timing = @{
        agent       = $name
        scenario    = $scenario
        run         = $run
        start_ms    = $t.startEpochMs
        total_ms    = $sw.ElapsedMilliseconds
    } | ConvertTo-Json
    Set-Content -Path (Join-Path $runsDir "${scenario}_run${run}_${name}_timing.json") -Value $timing
    Save-Captures $name $scenario $run
    Start-Sleep -Milliseconds 500
}

foreach ($scenario in $Scenarios) {
    # s1 keeps the read prompt; code scenarios get the write prompt.
    $activePrompt = if ($scenario -in @('s2_codegen', 's3_multistep', 's4_long')) { $codegenPrompt } else { $prompt }
    if ($ExtraPrompt) { $activePrompt = "$activePrompt $ExtraPrompt" }
    $suffix = if ($VariantTag) { "_$VariantTag" } else { '' }

    foreach ($run in 1..$Runs) {

        if ($Agents -contains 'slim') {
            Run-Agent "slim$suffix" $scenario $run {
                Set-Location $using:taskDir
                $env:SLIM_API_KEY = 'bench-key'
                $env:SLIM_EFFORT = $using:thinking
                & $using:SlimPath --headless --provider openai `
                    --endpoint "http://127.0.0.1:$using:Port/v1/chat/completions" `
                    --model $using:model --prompt $using:activePrompt --jsonl 2>&1
            }
        }

        if ($Agents -contains 'pi') {
            Run-Agent "pi$suffix" $scenario $run {
                Set-Location $using:taskDir
                $env:PI_CODING_AGENT_DIR = Join-Path $using:envRoot 'pi-agent'
                & pi -p $using:activePrompt --provider tokenbench --model $using:model `
                    --api-key bench-key --thinking $using:thinking `
                    --no-session --no-context-files --no-extensions `
                    --no-skills --no-prompt-templates --offline --mode text 2>&1
            }
        }

        if ($Agents -contains 'pit') {
            Run-Agent 'pit' $scenario $run {
                Set-Location $using:taskDir
                $env:PIT_CODING_AGENT_DIR = Join-Path $using:envRoot 'pit-agent'
                # Disable Pit's adaptive thinking downshift (turn after a clean
                # tool result runs at "low") so every turn honors --thinking high.
                $env:PIT_NO_ADAPTIVE_THINKING = '1'
                # Invoked via node directly: bin\pit.ps1 sets ErrorActionPreference=Stop,
                # which turns the tsx stderr notice into a terminating error inside jobs.
                & node 'C:\PiTest\bin\pit.mjs' -p $using:activePrompt --provider tokenbench `
                    --model $using:model --api-key bench-key --thinking $using:thinking `
                    --no-session --no-context-files --no-extensions --no-skills `
                    --no-prompt-templates --max-wall 180 2>&1
            }
        }
    }
}

Write-Host '=== done; run: python bench/token-economy/v2/analyze.py ==='
