$ErrorActionPreference = 'Stop'
$searchRoot = Join-Path $env:TEMP ('slim-search-poc-' + [DateTime]::UtcNow.ToString('yyyyMMddHHmmssfff'))
$srcRoot = Join-Path $searchRoot 'src'
$ignoredRoot = Join-Path $searchRoot 'ignored'
[IO.Directory]::CreateDirectory($srcRoot) | Out-Null
[IO.Directory]::CreateDirectory($ignoredRoot) | Out-Null
Set-Content -LiteralPath (Join-Path $searchRoot '.gitignore') -Value 'ignored/' -Encoding UTF8
$unicode = [string]::Concat([char]0x00E9, [char]0x041F, [char]0x0440, [char]0x0438, [char]0x0432, [char]0x0435, [char]0x0442)
for ($index = 0; $index -lt 1000; $index++) {
    $value = if ($index -eq 777) { "needle Unicode $unicode" } else { 'ordinary line' }
    Set-Content -LiteralPath (Join-Path $srcRoot ("file-$index.txt")) -Value $value -Encoding UTF8
}
[IO.File]::WriteAllBytes((Join-Path $srcRoot 'binary.bin'), [byte[]](0, 1, 2, 3, 255, 0))
Set-Content -LiteralPath (Join-Path $ignoredRoot 'hidden.txt') -Value 'needle ignored' -Encoding UTF8
$rg = (Get-Command rg.exe -ErrorAction SilentlyContinue).Source
$fff = Get-Command fff-search.exe -ErrorAction SilentlyContinue
$ffgrep = Get-Command ffgrep.exe -ErrorAction SilentlyContinue
$firstWatch = [Diagnostics.Stopwatch]::StartNew()
$firstHits = & $rg -l --hidden --glob '!.gitignore' --glob '!ignored/**' --glob '!**/ignored/**' 'needle' $searchRoot 2>$null
$firstWatch.Stop()
$warmWatch = [Diagnostics.Stopwatch]::StartNew()
for ($index = 0; $index -lt 100; $index++) {
    & $rg -l --hidden --glob '!.gitignore' --glob '!ignored/**' --glob '!**/ignored/**' 'needle' $searchRoot 2>$null | Out-Null
}
$warmWatch.Stop()
$renamed = Join-Path $srcRoot 'renamed.txt'
Copy-Item -LiteralPath (Join-Path $srcRoot 'file-777.txt') -Destination $renamed
$renameHits = & $rg -l 'needle' $renamed 2>$null
[PSCustomObject]@{
    root = $searchRoot
    ripgrep = $rg
    fff_search = if ($fff) { $fff.Source } else { 'NOT_FOUND' }
    ffgrep = if ($ffgrep) { $ffgrep.Source } else { 'NOT_FOUND' }
    first_ms = [Math]::Round($firstWatch.Elapsed.TotalMilliseconds, 3)
    hundred_ms = [Math]::Round($warmWatch.Elapsed.TotalMilliseconds, 3)
    warm_p50_estimate_ms = [Math]::Round($warmWatch.Elapsed.TotalMilliseconds / 100, 3)
    hit_count = @($firstHits).Count
    first_hits = @($firstHits)
    correct = (@($firstHits).Count -eq 1 -and (@($firstHits) -join '') -match 'file-777')
    copied_rename_hit = (@($renameHits).Count -eq 1)
} | ConvertTo-Json -Depth 3
