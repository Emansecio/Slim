#Requires -Version 5
<#
.SYNOPSIS
    Validacao direcionada, com baixa prioridade e timings de compilacao.
.EXAMPLE
    .\test-slim.ps1 -Package slim-core -Lib -Filter runtime::shell_jobs
.EXAMPLE
    .\test-slim.ps1 -Package slim-core -TestTarget agent_loop,native_tool_recovery
.EXAMPLE
    .\test-slim.ps1 -Workspace
#>
[CmdletBinding(DefaultParameterSetName = 'Target')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Target')]
    [ValidateSet('slim-core','slim-cli','slim-tui','slim-lsp')]
    [string[]]$Package,
    [Parameter(ParameterSetName = 'Target')][switch]$Lib,
    [Parameter(ParameterSetName = 'Target')][string[]]$TestTarget,
    [Parameter(ParameterSetName = 'Target')][string]$Filter,
    [Parameter(Mandatory, ParameterSetName = 'Workspace')][switch]$Workspace,
    [switch]$FullDebug,
    [ValidateRange(1, 64)][int]$TestThreads = 2
)
$ErrorActionPreference = 'Stop'
if ($PSCmdlet.ParameterSetName -eq 'Target' -and -not $Lib -and -not $TestTarget) {
    throw 'Escolha -Lib e/ou -TestTarget para evitar compilar alvos desnecessarios.'
}
. "$PSScriptRoot/cargo-local.ps1"
$cargoArguments = @('test', '--jobs', '1', '--timings')
if ($FullDebug) {
    foreach ($item in @('slim-core', 'slim-cli', 'slim-lsp', 'slim-tui')) {
        $cargoArguments += @('--config', "profile.test.package.$item.debug=2")
    }
}
if ($Workspace) { $cargoArguments += @('--workspace', '--no-fail-fast') }
else {
    foreach ($item in ($Package | Select-Object -Unique)) {
        $cargoArguments += @('-p', $item)
    }
    if ($Lib) { $cargoArguments += '--lib' }
    foreach ($target in $TestTarget) {
        if ([string]::IsNullOrWhiteSpace($target) -or $target.StartsWith('-')) {
            throw 'Nome de alvo de teste invalido.'
        }
        $cargoArguments += @('--test', $target)
    }
    if ($Filter) { $cargoArguments += $Filter }
}
$cargoArguments += @('--', '--test-threads', "$TestThreads")
# Isolate runtime configuration and credentials without changing the toolchain's home.
# Tests that need these settings provide their own fixtures in child processes.
$isolatedNames = @('SLIM_CONFIG_FILE', 'SLIM_AUTH_FILE', 'SLIM_API_KEY',
    'OPENAI_API_KEY', 'CODEX_ACCESS_TOKEN', 'ANTHROPIC_API_KEY', 'OPENCODE_API_KEY',
    'CLINEPASS_API_KEY', 'COMMANDCODE_API_KEY', 'CMD_API_KEY', 'XAI_API_KEY',
    'TYPESAFE_API_KEY', 'AI_GATEWAY_API_KEY',
    # Release-only revision; tracked by option_env!, so leaking it rebuilds slim-cli tests.
    'SLIM_BUILD_REVISION')
$savedEnvironment = @{}
$testRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('slim-test-env-' + [guid]::NewGuid().ToString('N'))
try {
    foreach ($name in $isolatedNames) {
        $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $null, 'Process')
    }
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    $env:SLIM_CONFIG_FILE = Join-Path $testRoot 'slim.toml'
    $env:SLIM_AUTH_FILE = Join-Path $testRoot 'auth.json'
    Invoke-SlimCargo -CargoArguments $cargoArguments
}
finally {
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    $resolvedRoot = [System.IO.Path]::GetFullPath($testRoot)
    $tempParent = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()).TrimEnd('\', '/')
    if ((Split-Path $resolvedRoot -Parent) -ne $tempParent -or
        (Split-Path $resolvedRoot -Leaf) -notmatch '^slim-test-env-[a-f0-9]{32}$') {
        throw 'Diretorio temporario de testes fora do limite esperado.'
    }
    if (Test-Path -LiteralPath $resolvedRoot) {
        Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
    }
}
