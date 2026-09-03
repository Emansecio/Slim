# Requires -Version 5
<#
.SYNOPSIS
    Hermetic Slim x Pi x Pit benchmark runner.

.DESCRIPTION
    Starts a deterministic local fixture, creates a fresh campaign directory,
    runs each arm in an isolated workspace, and fails on process, request-count,
    or model/effort compliance errors. No live provider is called.
#>
param(
    [string]$SlimPath = "$env:USERPROFILE\bin\Slim.exe",
    [string]$PiPath = "",
    [string]$PitPath = "C:\PiTest\bin\pit.mjs",
    [int]$Port = 8931,
    [ValidateRange(1, 1000)] [int]$Runs = 3,
    [ValidateSet('s1_read', 's2_codegen', 's3_multistep', 's4_long')]
    [string[]]$Scenarios = @('s1_read', 's2_codegen', 's3_multistep', 's4_long'),
    [ValidateSet('slim', 'pi', 'pit')]
    [string[]]$Agents = @('slim', 'pi', 'pit'),
    [string]$Campaign = '',
    [string]$VariantTag = '',
    [string]$ExtraPrompt = '',
    [int]$TimeoutSeconds = 240,
    [switch]$NoAnalyze
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$bench = $PSScriptRoot
$model = 'gpt-5.6-luna'
$thinking = 'high'
$expectedRequests = @{ s1_read = 2; s2_codegen = 2; s3_multistep = 3; s4_long = 5 }

function Resolve-Executable([string]$explicit, [string]$fallback) {
    if ($explicit) {
        if (-not (Test-Path -LiteralPath $explicit)) { throw "executable not found: $explicit" }
        return (Resolve-Path -LiteralPath $explicit).Path
    }
    return (Get-Command $fallback -ErrorAction Stop | Select-Object -First 1).Source
}

function Get-CommandVersion([string]$path, [string[]]$arguments) {
    try {
        $value = & $path @arguments 2>$null | Select-Object -First 1
        if ($value) { return [string]$value }
    } catch {}
    return $null
}

function Write-Utf8NoBom([string]$path, [string]$value) {
    [IO.File]::WriteAllText($path, $value, (New-Object Text.UTF8Encoding($false)))
}

function Write-Json([string]$path, $value) {
    Write-Utf8NoBom $path (($value | ConvertTo-Json -Depth 12) + "`n")
}

function Wait-Fixture([int]$port, [System.Diagnostics.Process]$process) {
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($process.HasExited) { throw "capture server exited with code $($process.ExitCode)" }
        try {
            Invoke-RestMethod -Method Get -Uri "http://127.0.0.1:$port/health" -TimeoutSec 1 | Out-Null
            return
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    throw "capture server did not become ready on port $port"
}

function Reset-Capture([string]$scenario, [string]$tag, [string]$run) {
    $uri = "http://127.0.0.1:$Port/reset?scenario=$scenario&tag=$tag&run=$run"
    Invoke-RestMethod -Method Post -ContentType 'application/json' -Uri $uri -Body '{}' | Out-Null
}

function New-Workspace([string]$root) {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $lines = 1..40 | ForEach-Object {
        "line-$_ : the quick brown fox jumps over the lazy dog 0x{0:x4}" -f $_
    }
    [IO.File]::WriteAllText(
        (Join-Path $root 'bench-target.txt'),
        (($lines -join "`r`n") + "`r`n"),
        (New-Object Text.UTF8Encoding($false))
    )
}

function Invoke-AgentProcess(
    [string]$tag,
    [string]$scenario,
    [int]$run,
    [string]$fileName,
    [string[]]$arguments,
    [string]$workingDirectory,
    [hashtable]$environment
) {
    $base = "${scenario}_run${run}_${tag}"
    $specPath = Join-Path $runsDir "$base.spec.json"
    $timingPath = Join-Path $runsDir "${base}_timing.json"
    $stdoutPath = Join-Path $runsDir "$base.stdout.log"
    $stderrPath = Join-Path $runsDir "$base.stderr.log"
    Write-Json $specPath ([ordered]@{
        agent = $tag
        scenario = $scenario
        run = $run
        executable = $fileName
        arguments = @($arguments)
        cwd = $workingDirectory
        environment_keys = @($environment.Keys | Sort-Object)
        environment = $environment
        timeout_seconds = $TimeoutSeconds
    })
    & $python (Join-Path $bench 'process_runner.py') $specPath $timingPath $stdoutPath $stderrPath
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $timingPath)) {
        throw "$tag process runner failed"
    }
    return Get-Content -LiteralPath $timingPath -Raw | ConvertFrom-Json
}

function Assert-Arm([string]$scenario, [string]$run, [string]$tag, $timing) {
    $arm = Join-Path $captures "$scenario\run$run\$tag"
    $requests = @(Get-ChildItem -LiteralPath $arm -File -Filter 'req_*.json' -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -notlike '*.meta.json' })
    $metas = @(Get-ChildItem -LiteralPath $arm -File -Filter 'req_*.meta.json' -ErrorAction SilentlyContinue)
    $compliancePath = Join-Path $arm 'compliance.jsonl'
    $compliance = @()
    if (Test-Path -LiteralPath $compliancePath) {
        $compliance = @(Get-Content -LiteralPath $compliancePath | Where-Object { $_.Trim() } |
            ForEach-Object { $_ | ConvertFrom-Json })
    }
    $expected = $expectedRequests[$scenario]
    $problems = @()
    if ($timing.timed_out) { $problems += 'timed out' }
    if ($null -eq $timing.exit_code -or $timing.exit_code -ne 0) { $problems += "exit code $($timing.exit_code)" }
    if ($requests.Count -ne $expected) { $problems += "requests $($requests.Count)/$expected" }
    if ($metas.Count -ne $expected) { $problems += "timing metas $($metas.Count)/$expected" }
    if ($compliance.Count -ne $expected) { $problems += "compliance records $($compliance.Count)/$expected" }
    if (@($compliance | Where-Object { -not $_.ok }).Count -gt 0) { $problems += 'model/effort violation' }
    if ($problems.Count -gt 0) { throw "$scenario/run$run/$tag failed gate: $($problems -join ', ')" }
}

$python = Resolve-Executable '' 'python.exe'
$node = Resolve-Executable '' 'node.exe'
if ($Agents -contains 'slim') { $SlimPath = Resolve-Executable $SlimPath 'Slim.exe' }
if ($Agents -contains 'pi') { $PiPath = Resolve-Executable $PiPath 'pi.cmd' }
if ($Agents -contains 'pit') {
    if (-not (Test-Path -LiteralPath $PitPath)) { throw "Pit entrypoint not found: $PitPath" }
    $PitPath = (Resolve-Path -LiteralPath $PitPath).Path
}

if (-not $Campaign) { $Campaign = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmssZ') }
if ($Campaign -notmatch '^[A-Za-z0-9_.-]+$') { throw 'invalid campaign name' }
$campaignRoot = Join-Path $bench "campaigns\$Campaign"
if (Test-Path -LiteralPath $campaignRoot) { throw "campaign already exists: $campaignRoot" }
$captures = Join-Path $campaignRoot 'captures'
$runsDir = Join-Path $campaignRoot 'runs'
$workspaces = Join-Path $campaignRoot 'workspaces'
$configRoot = Join-Path $campaignRoot 'agent-dirs'
New-Item -ItemType Directory -Force -Path $captures, $runsDir, $workspaces, $configRoot | Out-Null

$models = @{
    providers = @{
        tokenbench = @{
            baseUrl = "http://127.0.0.1:$Port/v1"
            api = 'openai-completions'
            apiKey = 'bench-key'
            models = @(@{
                id = $model
                reasoning = $true
                thinkingLevelMap = @{ high = $thinking }
                compat = @{ supportsDeveloperRole = $false }
            })
        }
    }
} | ConvertTo-Json -Depth 8

$manifest = [ordered]@{
    schema_version = 3
    campaign = $Campaign
    created_utc = [DateTime]::UtcNow.ToString('o')
    host = [ordered]@{
        os = [Environment]::OSVersion.VersionString
        powershell = $PSVersionTable.PSVersion.ToString()
        python = Get-CommandVersion $python @('--version')
        node = Get-CommandVersion $node @('--version')
    }
    model = $model
    effort = $thinking
    runs = $Runs
    scenarios = @($Scenarios)
    agents = @($Agents)
    order_policy = 'rotating-per-run'
    isolation = 'workspace and agent configuration per scenario/run/agent'
    expected_requests_by_scenario = $expectedRequests
    extra_prompt = $ExtraPrompt
    variant_tag = $VariantTag
    executables = [ordered]@{}
}
if ($Agents -contains 'slim') {
    $manifest.executables.slim = [ordered]@{
        path = $SlimPath
        sha256 = (Get-FileHash -LiteralPath $SlimPath -Algorithm SHA256).Hash.ToLowerInvariant()
        version = Get-CommandVersion $SlimPath @('--version')
    }
}
if ($Agents -contains 'pi') {
    $manifest.executables.pi = [ordered]@{
        path = $PiPath
        sha256 = (Get-FileHash -LiteralPath $PiPath -Algorithm SHA256).Hash.ToLowerInvariant()
        version = Get-CommandVersion $PiPath @('--version')
    }
}
if ($Agents -contains 'pit') {
    $previousQuiet = [Environment]::GetEnvironmentVariable('PIT_LAUNCH_QUIET', 'Process')
    try {
        [Environment]::SetEnvironmentVariable('PIT_LAUNCH_QUIET', '1', 'Process')
        $pitVersion = Get-CommandVersion $node @($PitPath, '--version')
    } finally {
        [Environment]::SetEnvironmentVariable('PIT_LAUNCH_QUIET', $previousQuiet, 'Process')
    }
    $manifest.executables.pit = [ordered]@{
        path = $PitPath
        sha256 = (Get-FileHash -LiteralPath $PitPath -Algorithm SHA256).Hash.ToLowerInvariant()
        version = $pitVersion
    }
}

Write-Json (Join-Path $campaignRoot 'manifest.json') $manifest

$serverOut = Join-Path $campaignRoot 'capture-server.stdout.log'
$serverErr = Join-Path $campaignRoot 'capture-server.stderr.log'
$server = Start-Process -FilePath $python -ArgumentList @(
    (Join-Path $bench 'capture_server.py'), '--port', $Port, '--out', $captures
) -WorkingDirectory $bench -RedirectStandardOutput $serverOut -RedirectStandardError $serverErr -PassThru -WindowStyle Hidden

$succeeded = $false
try {
    Wait-Fixture $Port $server
    foreach ($scenario in $Scenarios) {
        $basePrompt = if ($scenario -in @('s2_codegen', 's3_multistep', 's4_long')) {
            'Create fizzbuzz.py implementing fizzbuzz up to 25.'
        } else {
            'Read bench-target.txt and tell me its first line.'
        }
        $activePrompt = if ($ExtraPrompt) { "$basePrompt $ExtraPrompt" } else { $basePrompt }
        for ($run = 1; $run -le $Runs; $run++) {
            $runConfigRoot = Join-Path $configRoot "$scenario\run$run"
            foreach ($name in @('pi', 'pit')) {
                $dir = Join-Path $runConfigRoot "$name-agent"
                New-Item -ItemType Directory -Force -Path $dir | Out-Null
                Write-Utf8NoBom (Join-Path $dir 'models.json') ($models + "`n")
            }
            $slimConfig = Join-Path $runConfigRoot 'slim.toml'
            Write-Utf8NoBom $slimConfig "effort = `"high`"`n"
            $orderedAgents = @($Agents)
            if ($orderedAgents.Count -gt 1) {
                $shift = ($run - 1) % $orderedAgents.Count
                $orderedAgents = @(
                    for ($index = 0; $index -lt $orderedAgents.Count; $index++) {
                        $orderedAgents[($index + $shift) % $orderedAgents.Count]
                    }
                )
            }
            foreach ($agent in $orderedAgents) {
                $tag = if ($VariantTag) { "${agent}_$VariantTag" } else { $agent }
                Write-Host "=== $scenario / run $run / $tag ==="
                $workspace = Join-Path $workspaces "$scenario\run$run\$tag"
                New-Workspace $workspace
                Reset-Capture $scenario $tag "$run"
                $environment = @{}
                switch ($agent) {
                    'slim' {
                        $fileName = $SlimPath
                        $arguments = @(
                            '--headless', '--provider', 'openai',
                            '--endpoint', "http://127.0.0.1:$Port/v1/chat/completions",
                            '--model', $model, '--prompt', $activePrompt, '--jsonl'
                        )
                        $environment.SLIM_API_KEY = 'bench-key'
                        $environment.SLIM_EFFORT = $thinking
                        $environment.SLIM_CONFIG_FILE = $slimConfig
                    }
                    'pi' {
                        $fileName = $PiPath
                        $arguments = @(
                            '-p', $activePrompt, '--provider', 'tokenbench', '--model', $model,
                            '--api-key', 'bench-key', '--thinking', $thinking,
                            '--no-session', '--no-context-files', '--no-extensions', '--no-skills',
                            '--no-prompt-templates', '--no-themes', '--offline', '--mode', 'text'
                        )
                        $environment.PI_CODING_AGENT_DIR = Join-Path $runConfigRoot 'pi-agent'
                        $environment.PI_OFFLINE = '1'
                    }
                    'pit' {
                        $fileName = $node
                        $arguments = @(
                            $PitPath, '-p', $activePrompt, '--provider', 'tokenbench', '--model', $model,
                            '--api-key', 'bench-key', '--thinking', $thinking,
                            '--no-session', '--no-context-files', '--no-extensions', '--no-skills',
                            '--no-prompt-templates', '--max-wall', [string]([Math]::Min($TimeoutSeconds, 180))
                        )
                        $environment.PIT_CODING_AGENT_DIR = Join-Path $runConfigRoot 'pit-agent'
                        $environment.PIT_NO_ADAPTIVE_THINKING = '1'
                    }
                }
                $timing = Invoke-AgentProcess $tag $scenario $run $fileName $arguments $workspace $environment
                Assert-Arm $scenario "$run" $tag $timing
            }
        }
    }
    $succeeded = $true
} finally {
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force }
}

if (-not $succeeded) { throw 'benchmark campaign failed' }
Write-Host "=== campaign passed gates: $campaignRoot ==="
if (-not $NoAnalyze) {
    & $python (Join-Path $bench 'analyze.py') $campaignRoot
    if ($LASTEXITCODE -ne 0) { throw "analyzer failed with exit code $LASTEXITCODE" }
}
