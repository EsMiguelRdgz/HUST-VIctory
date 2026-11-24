#Requires -Version 5.1
<#
.SYNOPSIS
    CPU-only self-test of the diagnostic harness (no GPU, no title).
.DESCRIPTION
    Checks VictoryDiagnosticLib.ps1 with synthetic inputs (preflight
    decisions, skip-aware cargo output parsing, PID-reuse-safe descendant
    collection, debug-layer verdicts), terminates a real process tree, runs
    Run-VictoryDiagnostic.ps1 end to end in -SyntheticCpuOnly mode (timeout
    with a grandchild; skipped dependencies and an unverifiable debug layer
    reported), and checks that tools/ci/Test-HardwareSafety.ps1 rejects
    harness copies with the new guards removed. Exits 1 on any failure.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = '',
    [string]$WorkDir = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# $PSScriptRoot is empty inside param() defaults on Windows PowerShell 5.1.
if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path }
if ([string]::IsNullOrWhiteSpace($WorkDir)) {
    $WorkDir = Join-Path ([IO.Path]::GetTempPath()) ("victory-diag-selftest-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
}
. (Join-Path $PSScriptRoot 'VictoryDiagnosticLib.ps1')

$script:failures = New-Object System.Collections.Generic.List[string]
$script:passes = 0
function Assert-That([bool]$Condition, [string]$What) {
    if ($Condition) { $script:passes++; Write-Host "  ok   $What" }
    else { $script:failures.Add($What); Write-Host "  FAIL $What" }
}
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

# ---------------------------------------------------------------------------
Write-Host 'Preflight decisions'
$healthy = @([PSCustomObject]@{ Name = 'NVIDIA GeForce RTX 3070'; Status = 'OK'; ConfigManagerErrorCode = 0 })
$base = @{ Adapters = $healthy; RecentTdrEvents = @(); RunningProcessNames = @('explorer', 'notepad'); CommitHeadroomMB = 20000; MinPreLaunchHeadroomGB = 10; RecoveryIntervalSeconds = 60 }
function Invoke-Preflight([hashtable]$Over) {
    $p = $base.Clone()
    foreach ($k in $Over.Keys) { $p[$k] = $Over[$k] }
    Get-VictoryPreflightProblems @p
}
$r = Invoke-Preflight @{}
Assert-That (@($r.Problems).Count -eq 0 -and $r.WaitSeconds -eq 0) 'healthy machine passes'
$r = Invoke-Preflight @{ Adapters = @([PSCustomObject]@{ Name = 'Microsoft Basic Render Driver'; Status = 'OK'; ConfigManagerErrorCode = 0 }) }
Assert-That (@($r.Problems) -match 'no NVIDIA').Count 'no NVIDIA adapter refused'
$r = Invoke-Preflight @{ Adapters = @([PSCustomObject]@{ Name = 'NVIDIA GeForce RTX 3070'; Status = 'Error'; ConfigManagerErrorCode = 43 }) }
Assert-That (@($r.Problems) -match 'unhealthy').Count 'unhealthy NVIDIA adapter refused'
$r = Invoke-Preflight @{ RecentTdrEvents = @([PSCustomObject]@{ Id = 4101 }) }
Assert-That (@($r.Problems) -match '4101').Count 'recent display driver reset refused'
$r = Invoke-Preflight @{ RunningProcessNames = @('victory_gpu-45d663540fdb340b') }
Assert-That (@($r.Problems) -match 'already running').Count 'running cargo test binary refused'
$r = Invoke-Preflight @{ RunningProcessNames = @('game_scarlett_master') }
Assert-That (@($r.Problems) -match 'already running').Count 'running title refused'
$r = Invoke-Preflight @{ CommitHeadroomMB = 9000 }
Assert-That (@($r.Problems) -match 'pre-launch floor').Count 'headroom below 10 GB refused'
$now = [datetime]::UtcNow
$r = Invoke-Preflight @{ LastLaunchEndUtc = $now.AddSeconds(-20); NowUtc = $now }
Assert-That ($r.WaitSeconds -eq 40) "recovery interval waits the remainder (got $($r.WaitSeconds))"

# ---------------------------------------------------------------------------
Write-Host 'Cargo output parsing'
$text = @'
     Running unittests src\lib.rs (target\debug\deps\victory_gpu-1.exe)
test resource::tests::bench_texture_staging ... ignored
test host::tests::a ... ok
test result: ok. 113 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 2.42s
no console compiler; skipped
no SM 6.6 hardware device; skipped
test emit::valu_tests::b ... FAILED
test result: FAILED. 109 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
'@
$s = Get-CargoTestSummary -Text $text
Assert-That ($s.Passed -eq 222 -and $s.Failed -eq 1 -and $s.Ignored -eq 4 -and $s.ResultLines -eq 2) "totals summed across binaries ($($s.Passed)/$($s.Failed)/$($s.Ignored))"
Assert-That ($s.SkippedCount -eq 2) "skipped dependencies reported ($($s.SkippedCount))"
Assert-That (@($s.FailedTests) -contains 'emit::valu_tests::b') 'failed test named'
Assert-That (@($s.IgnoredTests) -contains 'resource::tests::bench_texture_staging') 'ignored test named, not counted as a skip'

# ---------------------------------------------------------------------------
Write-Host 'Descendant collection (synthetic snapshot)'
$t0 = Get-Date '2026-11-24T10:00:00'
$snap = @(
    [PSCustomObject]@{ ProcessId = 100; ParentProcessId = 1; CreationDate = $t0 },
    [PSCustomObject]@{ ProcessId = 200; ParentProcessId = 100; CreationDate = $t0.AddSeconds(1) },
    [PSCustomObject]@{ ProcessId = 300; ParentProcessId = 200; CreationDate = $t0.AddSeconds(2) },
    [PSCustomObject]@{ ProcessId = 400; ParentProcessId = 999; CreationDate = $t0 },
    # Claims parent 100 but predates it: a reused PID, not our child.
    [PSCustomObject]@{ ProcessId = 500; ParentProcessId = 100; CreationDate = $t0.AddSeconds(-30) }
)
$ids = @(Get-VictoryDescendants -RootId 100 -Snapshot $snap | ForEach-Object { [int]$_.ProcessId } | Sort-Object)
Assert-That (($ids -join ',') -eq '200,300') "children and grandchildren only ($($ids -join ','))"
# Root 100 exited at t0+5 and its PID was reused at t0+60 by a process whose child appeared at t0+61.
$snap2 = @(
    [PSCustomObject]@{ ProcessId = 100; ParentProcessId = 1; CreationDate = $t0.AddSeconds(60) },
    [PSCustomObject]@{ ProcessId = 200; ParentProcessId = 100; CreationDate = $t0.AddSeconds(1) },
    [PSCustomObject]@{ ProcessId = 600; ParentProcessId = 100; CreationDate = $t0.AddSeconds(61) }
)
$ids = @(Get-VictoryDescendants -RootId 100 -Snapshot $snap2 -RootCreationDate $t0 -RootChildrenNotAfter $t0.AddSeconds(5) | ForEach-Object { [int]$_.ProcessId })
Assert-That (($ids -join ',') -eq '200') "orphan kept, reused-PID child excluded ($($ids -join ','))"

# ---------------------------------------------------------------------------
Write-Host 'Debug-layer verdicts'
$recs = @(
    [PSCustomObject]@{ probe = 'd3d12-debug-layer'; crate = 'victory-gpu'; supported = $true; requested = $true; active = $true },
    [PSCustomObject]@{ probe = 'd3d12-debug-layer'; crate = 'victory-shader'; supported = $false; requested = $true; active = $false }
)
$v = Get-VictoryDebugLayerVerdict -Records $recs -Requested $true
Assert-That ($v.'victory-gpu' -eq 'Active' -and $v.'victory-shader' -eq 'NotEnabledByTestHost') 'active and not-enabled reported per crate'
$v = Get-VictoryDebugLayerVerdict -Records @([PSCustomObject]@{ probe = 'd3d12-debug-layer'; crate = 'victory-gpu'; supported = $true; requested = $true; active = $false }) -Requested $true
Assert-That ($v.'victory-gpu' -eq 'NotActive' -and $v.'victory-shader' -eq 'NoProbe') 'requested-but-inactive and missing probe reported'
$v = Get-VictoryDebugLayerVerdict -Records $recs -Requested $false
Assert-That ($v.'victory-gpu' -eq 'NotRequested') 'not requested reported'
$skips = @(Get-VictoryProbeSkips -Records @(
        [PSCustomObject]@{ probe = 'skip'; crate = 'victory-shader'; test = 'emit::diff_tests::x'; reason = 'no console compiler' },
        [PSCustomObject]@{ probe = 'adapter'; crate = 'victory-shader'; label = 'hardware'; unsupported = 'no SM 6.6' },
        [PSCustomObject]@{ probe = 'adapter'; crate = 'victory-shader'; label = 'warp'; adapter = 'Microsoft Basic Render Driver'; software = $true },
        [PSCustomObject]@{ probe = 'd3d12-debug-layer'; crate = 'victory-shader'; supported = $false; requested = $true; active = $false }))
Assert-That ($skips.Count -eq 2) "probe skips: skip records and unavailable adapters only ($($skips.Count))"

# ---------------------------------------------------------------------------
Write-Host 'Script encoding (Windows PowerShell 5.1 reads BOM-less files as ANSI)'
foreach ($ps in @(Get-ChildItem -LiteralPath (Join-Path $RepoRoot 'tools') -Recurse -File | Where-Object { $_.Extension -eq '.ps1' })) {
    $bytes = [IO.File]::ReadAllBytes($ps.FullName)
    $bom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
    $nonAscii = @($bytes | Where-Object { $_ -gt 127 }).Count -gt 0
    Assert-That ($bom -or -not $nonAscii) "$($ps.Name): ASCII or UTF-8 with BOM"
}

# ---------------------------------------------------------------------------
Write-Host 'Process-tree termination (real processes, CPU only)'
$pidFile = Join-Path $WorkDir 'grandchild.pid'
$inner = "`$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 300' -WindowStyle Hidden -PassThru; Set-Content -LiteralPath '$pidFile' -Value `$p.Id; Start-Sleep -Seconds 300"
$parent = Start-Process powershell.exe -ArgumentList '-NoProfile', '-Command', $inner -WindowStyle Hidden -PassThru
$deadline = (Get-Date).AddSeconds(30)
while (-not (Test-Path -LiteralPath $pidFile) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
$grand = if (Test-Path -LiteralPath $pidFile) { [int](Get-Content -LiteralPath $pidFile -Raw).Trim() } else { 0 }
Assert-That ($grand -gt 0) 'grandchild started'
$parentCreated = (Get-CimInstance Win32_Process -Filter "ProcessId = $($parent.Id)").CreationDate
$known = @{}
$stop = Stop-VictoryProcessTree -RootId $parent.Id -Known $known -RootCreationDate $parentCreated
$alive = @(Get-Process -Id $parent.Id, $grand -ErrorAction SilentlyContinue)
Assert-That ($alive.Count -eq 0 -and @($stop.Remaining).Count -eq 0) "parent and grandchild terminated ($(@($stop.Terminated) -join ','))"
Assert-That (@($stop.Terminated) -contains $grand) 'grandchild terminated explicitly'

# ---------------------------------------------------------------------------
Write-Host 'Harness end to end (-SyntheticCpuOnly, no GPU)'
$harness = Join-Path $PSScriptRoot 'Run-VictoryDiagnostic.ps1'
$results = Join-Path $WorkDir 'results'
New-Item -ItemType Directory -Force -Path $results | Out-Null
$pidFile2 = Join-Path $WorkDir 'e2e-grandchild.pid'
$cmd = "`$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 300' -WindowStyle Hidden -PassThru; Set-Content -LiteralPath '$pidFile2' -Value `$p.Id; Write-Output ('warp=' + `$env:VICTORY_D3D12_WARP + ' warp2=' + `$env:VICTORY_WARP); Start-Sleep -Seconds 300"
$out = & $harness -SyntheticCommand 'powershell.exe' -SyntheticArgs @('-NoProfile', '-Command', $cmd) -SyntheticCpuOnly `
    -RepoRoot $RepoRoot -ResultsRoot $results -MaxDurationSeconds 6 -SampleIntervalSeconds 1 -SkipBuild -NoDiscord -TestLabel 'selftest-timeout' 6>$null
$sum = @($out | Where-Object { $null -ne $_ -and $_.PSObject.Properties['RunDirectory'] }) | Select-Object -Last 1
Assert-That ($null -ne $sum) 'harness returned a summary'
if ($null -ne $sum) {
    Assert-That ($sum.Classification -eq 'MaxDurationReached') "timeout classified ($($sum.Classification))"
    Assert-That (-not $sum.HardwareRun -and $sum.Mode -eq 'synthetic-cpu') "CPU-only mode ($($sum.Mode))"
    Assert-That (@($sum.TreeRemaining).Count -eq 0) 'nothing left running'
    $g2 = if (Test-Path -LiteralPath $pidFile2) { [int](Get-Content -LiteralPath $pidFile2 -Raw).Trim() } else { 0 }
    Assert-That ($g2 -gt 0 -and @($sum.TreeTerminated) -contains $g2) "grandchild $g2 terminated on timeout ($(@($sum.TreeTerminated) -join ','))"
    Assert-That (@(Get-Process -Id $g2 -ErrorAction SilentlyContinue).Count -eq 0) 'grandchild no longer exists'
    $stdout = Get-Content -LiteralPath (Join-Path $sum.RunDirectory 'stdout.log') -Raw
    Assert-That ($stdout -match 'warp=1 warp2=1') 'CPU-only child forced onto the software adapter'
}

$skipCmd = "Write-Output 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'; Write-Output 'no console compiler; skipped'"
$out = & $harness -SyntheticCommand 'powershell.exe' -SyntheticArgs @('-NoProfile', '-Command', $skipCmd) -SyntheticCpuOnly `
    -RepoRoot $RepoRoot -ResultsRoot $results -MaxDurationSeconds 60 -SampleIntervalSeconds 1 -SkipBuild -NoDiscord -TestLabel 'selftest-skips' 6>$null
$sum = @($out | Where-Object { $null -ne $_ -and $_.PSObject.Properties['RunDirectory'] }) | Select-Object -Last 1
Assert-That ($null -ne $sum -and $sum.Classification -eq 'CleanExitWithSkips') "skipped dependency is not a silent clean exit ($($sum.Classification))"
Assert-That ($null -ne $sum -and $sum.TestsPassed -eq 3 -and @($sum.SkippedOrUnsupported).Count -eq 1) 'counts and skip line in the summary'

# A skip reported only through the probe file (cargo hides passing tests' stderr).
$probeCmd = "Write-Output 'test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'; Add-Content -LiteralPath `$env:VICTORY_TEST_PROBE_FILE -Value '{`"probe`":`"skip`",`"crate`":`"victory-shader`",`"test`":`"t`",`"reason`":`"no dxil.dll`"}'"
$out = & $harness -SyntheticCommand 'powershell.exe' -SyntheticArgs @('-NoProfile', '-Command', $probeCmd) -SyntheticCpuOnly `
    -RepoRoot $RepoRoot -ResultsRoot $results -MaxDurationSeconds 60 -SampleIntervalSeconds 1 -SkipBuild -NoDiscord -TestLabel 'selftest-probe-skip' 6>$null
$sum = @($out | Where-Object { $null -ne $_ -and $_.PSObject.Properties['RunDirectory'] }) | Select-Object -Last 1
Assert-That ($null -ne $sum -and $sum.Classification -eq 'CleanExitWithSkips' -and @($sum.SkippedByProbe).Count -eq 1) "probe-reported skip is not a silent clean exit ($($sum.Classification))"

$out = & $harness -SyntheticCommand 'powershell.exe' -SyntheticArgs @('-NoProfile', '-Command', $skipCmd) -SyntheticCpuOnly -EnableDebugLayer `
    -RepoRoot $RepoRoot -ResultsRoot $results -MaxDurationSeconds 60 -SampleIntervalSeconds 1 -SkipBuild -NoDiscord -TestLabel 'selftest-debug' 6>$null
$sum = @($out | Where-Object { $null -ne $_ -and $_.PSObject.Properties['RunDirectory'] }) | Select-Object -Last 1
Assert-That ($null -ne $sum -and $sum.Classification -eq 'DebugLayerNotVerified') "requested debug layer without a probe is not verified ($($sum.Classification))"

$refused = $null
try {
    # A hardware run (no -SyntheticCpuOnly): refused before any preflight or process.
    & $harness -SyntheticCommand 'cargo' -SyntheticArgs @('test', '--', '--ignored') `
        -RepoRoot $RepoRoot -ResultsRoot $results -SkipBuild -NoDiscord 6>$null | Out-Null
} catch {
    $refused = $_.Exception.Message
}
Assert-That ($null -ne $refused -and $refused -match '--ignored') 'ignored (known-defect) tests refused on the hardware adapter'

# ---------------------------------------------------------------------------
Write-Host 'Safety check rejects weakened harness copies'
$safety = Join-Path $RepoRoot 'tools\ci\Test-HardwareSafety.ps1'
function Test-MutatedHarness([string]$Name, [scriptblock]$Mutate) {
    $root = Join-Path $WorkDir "mut-$Name"
    New-Item -ItemType Directory -Force -Path (Join-Path $root 'tools\diag'), (Join-Path $root '.github\workflows') | Out-Null
    $text = [IO.File]::ReadAllText((Join-Path $RepoRoot 'tools\diag\Run-VictoryDiagnostic.ps1'))
    $mutated = & $Mutate $text
    [IO.File]::WriteAllText((Join-Path $root 'tools\diag\Run-VictoryDiagnostic.ps1'), $mutated)
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $safety -RepoRoot $root | Out-Null
    return [PSCustomObject]@{ Changed = ($mutated -ne $text); Exit = $LASTEXITCODE }
}
$m = Test-MutatedHarness 'none' { param($t) $t }
Assert-That ($m.Exit -eq 0) 'unmodified harness copy passes'
$m = Test-MutatedHarness 'cpuonly-default' { param($t) $t.Replace('$hardwareRun = -not $isSynthetic -or -not $SyntheticCpuOnly', '$hardwareRun = -not $isSynthetic') }
Assert-That ($m.Changed -and $m.Exit -ne 0) 'synthetic runs skipping the hardware preflight are rejected'
$m = Test-MutatedHarness 'no-tree-kill' { param($t) $t.Replace('reached; terminating the process tree."' + "`r`n" + '            $stop = Stop-VictoryProcessTree', 'reached; terminating the process tree."' + "`r`n" + '            $stop = $null; #') }
if (-not $m.Changed) { $m = Test-MutatedHarness 'no-tree-kill-lf' { param($t) $t.Replace('reached; terminating the process tree."' + "`n" + '            $stop = Stop-VictoryProcessTree', 'reached; terminating the process tree."' + "`n" + '            $stop = $null; #') } }
Assert-That ($m.Changed -and $m.Exit -ne 0) 'timeout without process-tree termination is rejected'

# ---------------------------------------------------------------------------
Remove-Item -LiteralPath $WorkDir -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
if ($script:failures.Count -gt 0) {
    Write-Host "diagnostic harness self-test: $($script:failures.Count) failure(s), $script:passes passed"
    $script:failures | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host "diagnostic harness self-test: $script:passes checks passed"
exit 0
