#Requires -Version 5
<#
.SYNOPSIS
    Prepara o Slim numa maquina limpa (Windows).

.DESCRIPTION
    Confere o toolchain Rust (nao instala), faz o build release do binario e,
    com -Deploy, copia para %USERPROFILE%\bin\Slim.exe (o caminho que o comando
    `slim` do terminal usa). Nao altera config global e nao cria slim.toml.

.EXAMPLE
    .\bootstrap.ps1
    .\bootstrap.ps1 -Deploy
    .\bootstrap.ps1 -Test -Deploy
#>
param(
    [switch]$Deploy,
    [switch]$Test
)
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo nao encontrado: instale Rust stable MSVC (rustup) com cargo no PATH.'
}
'cargo: ' + (cargo --version)
'rustc: ' + (rustc --version)

if ($Test) {
    '== cargo test --workspace =='
    Push-Location $root
    cargo test --workspace
    $code = $LASTEXITCODE
    Pop-Location
    if ($code -ne 0) { throw 'cargo test falhou' }
}

'== cargo build --release -p slim-cli =='
Push-Location $root
cargo build --release -p slim-cli
$code = $LASTEXITCODE
Pop-Location
if ($code -ne 0) { throw 'cargo build falhou' }

$built = Join-Path $root 'target\release\slim.exe'
if (-not (Test-Path $built)) { throw "binario nao encontrado: $built" }

if ($Deploy) {
    $deploy = Join-Path $env:USERPROFILE 'bin\Slim.exe'
    New-Item -ItemType Directory -Force -Path (Split-Path $deploy) | Out-Null
    Copy-Item $built $deploy -Force
    "deploy: $deploy"
    & $deploy --version
    if ($LASTEXITCODE -ne 0) { throw 'smoke test do binario implantado falhou' }
} else {
    "build ok: $built"
    "use -Deploy para copiar para $env:USERPROFILE\bin\Slim.exe"
}

''
'Falta fazer a mao:'
'  - /login na TUI (OAuth Anthropic/OpenAI) ou SLIM_API_KEY/OPENAI_API_KEY/ANTHROPIC_API_KEY'
'  - opcional: ./slim.toml ou %APPDATA%\slim\slim.toml'