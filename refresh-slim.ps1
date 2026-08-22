#Requires -Version 5
<#
.SYNOPSIS
    Build release do Slim e atualiza o binario implantado no PATH.

.DESCRIPTION
    O comando `slim` disponivel no terminal aponta para
    C:\Users\User\bin\Slim.exe — uma COPIA ESTATICA do binario, que NAO
    acompanha o codigo automaticamente. Este script e a unica forma
    suportada de build+deploy (regra obrigatoria em AGENTS.md):

        .\refresh-slim.ps1          build release + copia + smoke test
        .\refresh-slim.ps1 -Test    idem, rodando cargo test --workspace antes

    Tambem contorna as variaveis de usuario RUSTC/CARGO quebradas apontando
    RUSTC explicitamente para o toolchain ativo (rustc 1.97.1, scoop persist).
#>
param(
    [switch]$Test
)
$ErrorActionPreference = 'Stop'

$root   = $PSScriptRoot
$rustc  = 'C:\Users\User\scoop\persist\rustup-msvc\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rustc.exe'
$cargo  = 'C:\Users\User\scoop\apps\rustup-msvc\current\.cargo\bin\cargo.exe'
$deploy = Join-Path $env:USERPROFILE 'bin\Slim.exe'
$built  = Join-Path $root 'target\release\slim.exe'

if (-not (Test-Path $cargo)) { $cargo = 'cargo' }
if (Test-Path $rustc) { $env:RUSTC = $rustc }

if ($Test) {
    & $cargo test --workspace
    if ($LASTEXITCODE -ne 0) {
        Write-Error 'cargo test falhou — deploy abortado.'
        exit 1
    }
}

& $cargo build --release -p slim-cli
if ($LASTEXITCODE -ne 0) {
    Write-Error 'cargo build --release falhou — deploy abortado.'
    exit 1
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
