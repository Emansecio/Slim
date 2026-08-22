# Requires -Version 5
<#
.SYNOPSIS
    Token-economy benchmark: Slim x Pi x Pit against a local capture server.

.DESCRIPTION
    Assumes capture_server.py is already running on 127.0.0.1:8931.
    Runs the SAME task on each agent (one file read + final answer), captures
    every request body into captures\ and leaves analysis to analyze.py.

    Pi and Pit are pointed at isolated agent dirs through their official env
    overrides (PI_CODING_AGENT_DIR / PIT_CODING_AGENT_DIR); the user's real
    configuration is never touched.
#>
param(
    [string]$SlimPath = "$env:USERPROFILE\bin\Slim.exe",
    [int]$Port = 8931
)
$ErrorActionPreference = 'Continue'
$bench = $PSScriptRoot
$captures = Join-Path $bench 'captures'
$envRoot = Join-Path $bench '.agent-dirs'
New-Item -ItemType Directory -Force -Path $captures, $envRoot | Out-Null

# ---- fixed task -------------------------------------------------------------
$taskDir = Join-Path $bench '.task'
New-Item -ItemType Directory -Force -Path $taskDir | Out-Null
$lines = 1..40 | ForEach-Object { "line-$_ : the quick brown fox jumps over the lazy dog 0x{0:x4}" -f $_ }
Set-Content -Path (Join-Path $taskDir 'bench-target.txt') -Value ($lines -join "`r`n")
$prompt = 'Read bench-target.txt and tell me its first line.'

# ---- models.json for pi / pit ----------------------------------------------
$models = @{
    providers = @{
        tokenbench = @{
            baseUrl = "http://127.0.0.1:$Port/v1"
            api     = 'openai-completions'
            apiKey  = 'bench-key'
            models  = @(@{
                id     = 'bench-model'
                compat = @{ supportsDeveloperRole = $false; supportsReasoningEffort = $false }
            })
        }
    }
} | ConvertTo-Json -Depth 6

foreach ($name in @('pi', 'pit')) {
    $dir = Join-Path $envRoot "$name-agent"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Set-Content -Path (Join-Path $dir 'models.json') -Value $models
}

function Reset-Capture {
    Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:$Port/reset" | Out-Null
}

function Save-Captures([string]$agent) {
    Get-ChildItem $captures -Filter 'agent_req_*.json' -ErrorAction SilentlyContinue |
        ForEach-Object {
            Move-Item $_.FullName (Join-Path $captures ($agent + '_' + $_.Name)) -Force
        }
}

function Run-Agent([string]$name, [scriptblock]$invocation) {
    Write-Host "=== $name ==="
    Reset-Capture
    $job = Start-Job -ScriptBlock $invocation
    if (Wait-Job $job -Timeout 150) { Receive-Job $job | Out-Host }
    else { Write-Warning "$name timed out"; Stop-Job $job }
    Remove-Job $job -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    Save-Captures $name
}

# ---- Slim (OpenAI-compatible, same wire format as pi/pit) -------------------
Run-Agent 'slim' {
    Set-Location $using:taskDir
    $env:SLIM_API_KEY = 'bench-key'
    & $using:SlimPath --headless --provider openai `
        --endpoint "http://127.0.0.1:$using:Port/v1/chat/completions" `
        --model bench-model --prompt $using:prompt --jsonl 2>&1
}

# ---- Pi ---------------------------------------------------------------------
Run-Agent 'pi' {
    Set-Location $using:taskDir
    $env:PI_CODING_AGENT_DIR = Join-Path $using:envRoot 'pi-agent'
    & pi -p $using:prompt --provider tokenbench --model bench-model `
        --api-key bench-key --no-session --no-context-files --no-extensions `
        --no-skills --no-prompt-templates --no-themes --offline --mode text 2>&1
}

# ---- Pit --------------------------------------------------------------------
Run-Agent 'pit' {
    Set-Location $using:taskDir
    $env:PIT_CODING_AGENT_DIR = Join-Path $using:envRoot 'pit-agent'
    # Invoked via node directly: bin\pit.ps1 sets ErrorActionPreference=Stop,
    # which turns the tsx "src newer than bundle" stderr notice into a
    # terminating error inside jobs.
    & node 'C:\PiTest\bin\pit.mjs' -p $using:prompt --provider tokenbench `
        --model bench-model --api-key bench-key --no-session `
        --no-context-files --no-extensions --no-skills --no-prompt-templates `
        --max-wall 90 2>&1
}

Write-Host '=== done; run: python bench/token-economy/analyze.py ==='
