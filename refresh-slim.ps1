#Requires -Version 5
<#
.SYNOPSIS
    Build release do Slim e atualiza o binario implantado no PATH.

.DESCRIPTION
    O comando `slim` disponivel no terminal aponta para
    C:\Users\User\bin\Slim.exe — uma COPIA ESTATICA do binario, que NAO
    acompanha o codigo automaticamente. Este script e a unica forma
    suportada de build+deploy, e encerra o fluxo normal de edicao do Slim
    (AGENTS.md):

        .\refresh-slim.ps1          build release + copia + smoke test
        .\refresh-slim.ps1 -Test    idem, rodando cargo test --workspace antes
        .\refresh-slim.ps1 -FastBuild  build local mais rapido, com menos otimizacao

    Usa cargo/rustc do PATH. Variaveis RUSTC/CARGO herdadas que apontem para
    arquivos inexistentes sao removidas somente deste processo.
#>
param(
    [switch]$Test,
    [switch]$FastBuild
)
$ErrorActionPreference = 'Stop'

$root   = $PSScriptRoot
. "$root/cargo-local.ps1"
$cargo  = 'cargo'
$git    = 'git'
$deploy = Join-Path $env:USERPROFILE 'bin\Slim.exe'
$built  = Join-Path $root 'target\release\slim.exe'

if (-not (Get-Command $cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo nao encontrado no PATH.'
}
if (-not (Get-Command $git -ErrorAction SilentlyContinue)) {
    throw 'git nao encontrado no PATH.'
}
if ($env:RUSTC -and -not (Test-Path -LiteralPath $env:RUSTC -PathType Leaf)) {
    Remove-Item Env:RUSTC
}
if ($env:CARGO -and -not (Test-Path -LiteralPath $env:CARGO -PathType Leaf)) {
    Remove-Item Env:CARGO
}

$commit = (& $git -C $root rev-parse --verify HEAD 2>$null).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($commit)) {
    throw 'nao foi possivel identificar o commit HEAD para o build.'
}
$status = & $git -C $root status --porcelain --untracked-files=all
if ($LASTEXITCODE -ne 0) {
    throw 'nao foi possivel identificar se o worktree esta dirty.'
}
$dirtySuffix = if ([string]::IsNullOrWhiteSpace(($status -join "`n"))) { '' } else { '-dirty' }
$buildRevision = "$commit$dirtySuffix"

if ($Test) {
    & "$root/test-slim.ps1" -Workspace
}

$buildArguments = @('build', '--release', '-p', 'slim-cli', '--jobs', '1', '--timings')
if ($FastBuild) {
    # Preserve opt-level=3 while skipping cross-crate LTO and parallelizing codegen.
    # Opt in because runtime performance of this local profile is not benchmarked.
    $buildArguments += @('--config', 'profile.release.lto="off"',
        '--config', 'profile.release.codegen-units=8')
}
# option_env! tracks this variable: scope it to the release build so test builds
# in this shell keep their fingerprints.
$savedRevision = $env:SLIM_BUILD_REVISION
try {
    $env:SLIM_BUILD_REVISION = $buildRevision
    Write-Host "Build revision: $buildRevision"
    Invoke-SlimCargo -CargoArguments $buildArguments
}
finally {
    $env:SLIM_BUILD_REVISION = $savedRevision
}

New-Item -ItemType Directory -Force -Path (Split-Path $deploy) | Out-Null
Copy-Item $built $deploy -Force

$version = & $deploy --version
if ($LASTEXITCODE -ne 0 -or "$version" -notmatch '^slim 0\.1\.0') {
    Write-Error 'Smoke test do binario implantado falhou (slim --version).'
    exit 1
}

$stamp = (Get-Item $deploy).LastWriteTime
Write-Host "OK: Slim $version implantado em $deploy (build de $stamp)"
exit 0
