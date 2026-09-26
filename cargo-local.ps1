# Shared execution policy for local validation and publication.
function Invoke-SlimCargo {
    param([Parameter(Mandatory)][string[]]$CargoArguments)
    $process = [System.Diagnostics.Process]::GetCurrentProcess()
    $priority = $process.PriorityClass
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    Push-Location $PSScriptRoot
    try {
        # Child processes inherit the scheduling priority; this is not a CPU cap.
        $process.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal
        & cargo @CargoArguments
        $code = $LASTEXITCODE
        if ($code -ne 0) { throw "cargo terminou com exit $code." }
    }
    finally {
        $timer.Stop()
        $process.PriorityClass = $priority
        Pop-Location
        Write-Host ("Cargo ({0}): {1:n2}s" -f ($CargoArguments -join ' '), $timer.Elapsed.TotalSeconds)
    }
}
