param(
    [Parameter(Mandatory = $true)]
    [string]$Root
)

$rg = (Get-Command rg.exe -ErrorAction Stop).Source
$maxRss = 0L
$total = [Diagnostics.Stopwatch]::StartNew()
$firstMs = $null
for ($index = 0; $index -lt 100; $index++) {
    $out = Join-Path $env:TEMP ("slim-rg-$PID-$index.txt")
    $err = Join-Path $env:TEMP ("slim-rg-$PID-$index.err")
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $process = Start-Process -FilePath $rg -ArgumentList @('-l', '--hidden', 'needle', $Root) -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError $err -PassThru
    do {
        try {
            $current = (Get-Process -Id $process.Id -ErrorAction Stop).WorkingSet64
            if ($current -gt $maxRss) { $maxRss = $current }
        } catch { }
        Start-Sleep -Milliseconds 1
        $process.Refresh()
    } while (-not $process.HasExited)
    $watch.Stop()
    if ($index -eq 0) { $firstMs = $watch.Elapsed.TotalMilliseconds }
}
$total.Stop()
[PSCustomObject]@{
    first_ms = [Math]::Round($firstMs, 3)
    hundred_ms = [Math]::Round($total.Elapsed.TotalMilliseconds, 3)
    average_ms = [Math]::Round($total.Elapsed.TotalMilliseconds / 100, 3)
    max_rss_bytes = $maxRss
} | ConvertTo-Json
