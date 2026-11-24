#Requires -Version 5.1
<#
.SYNOPSIS
    Controlled, safety-guarded diagnostic harness for HUST-VIctory.
.DESCRIPTION
    Verifies Git branch/commit, builds the diagnostics executable, runs either a
    synthetic test command (-SyntheticCommand) or an explicitly authorized title
    executable (`victory run <exe> --execute` with -AuthorizeGameLaunch), samples
    process memory, Windows system commit headroom, and GPU memory counters,
    enforces max duration and >= 5 GB commit headroom safety cutoff, classifies
    stop reasons (memory guard vs D3D12 device removal vs timeout vs crash),
    writes timestamped reports under G:\VIctory-Results, and optionally sends a
    Discord completion notification.

    Never disables VICTORY_MEMORY_GUARD, never clears shader caches, never
    modifies pagefile/TDR settings, and never runs an automatic retry loop.
#>
[CmdletBinding(DefaultParameterSetName = 'Game')]
param(
    [Parameter(ParameterSetName = 'Game', Mandatory)]
    [string]$TitleExe,

    [Parameter(ParameterSetName = 'Game')]
    [switch]$AuthorizeGameLaunch,

    [Parameter(ParameterSetName = 'Game')]
    [ValidateSet('series-x', 'series-s')]
    [string]$Console = 'series-x',

    [Parameter(ParameterSetName = 'Game')]
    [string[]]$TitleArgs = @(),

    [Parameter(ParameterSetName = 'Synthetic', Mandatory)]
    [string]$SyntheticCommand,

    [Parameter(ParameterSetName = 'Synthetic')]
    [string[]]$SyntheticArgs = @(),

    [string]$RepoRoot = 'G:\VIctory',
    [string]$ResultsRoot = 'G:\VIctory-Results',
    [int]$MaxDurationSeconds = 600,
    [double]$MinCommitHeadroomGB = 5.0,
    [int]$SampleIntervalSeconds = 2,
    [string]$VictoryLog = 'info',
    [switch]$EnableCapture,
    [switch]$EnableDebugLayer,
    [switch]$EnableDred,
    [switch]$DisableLazyTextures,
    [switch]$SkipBuild,
    [switch]$NoDiscord
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Add Win32 GetPerformanceInfo for exact system commit/limit/physical readings.
if (-not ([System.Management.Automation.PSTypeName]'VictoryDiagPerf').Type) {
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

public static class VictoryDiagPerf {
    [StructLayout(LayoutKind.Sequential)]
    public struct PERFORMANCE_INFORMATION {
        public uint cb;
        public IntPtr CommitTotal;
        public IntPtr CommitLimit;
        public IntPtr CommitPeak;
        public IntPtr PhysicalTotal;
        public IntPtr PhysicalAvailable;
        public IntPtr SystemCache;
        public IntPtr KernelTotal;
        public IntPtr KernelPaged;
        public IntPtr KernelNonpaged;
        public IntPtr PageSize;
        public uint HandleCount;
        public uint ProcessCount;
        public uint ThreadCount;
    }

    [DllImport("psapi.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool GetPerformanceInfo(out PERFORMANCE_INFORMATION pPerformanceInformation, uint cb);

    public static PERFORMANCE_INFORMATION Query() {
        var info = new PERFORMANCE_INFORMATION();
        info.cb = (uint)Marshal.SizeOf(typeof(PERFORMANCE_INFORMATION));
        if (!GetPerformanceInfo(out info, info.cb)) {
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
        }
        return info;
    }
}
"@
}

function Get-SystemMemorySnapshot {
    $pi = [VictoryDiagPerf]::Query()
    $page = [double]$pi.PageSize.ToInt64()
    $commitTotalMB = ([double]$pi.CommitTotal.ToInt64() * $page) / 1MB
    $commitLimitMB = ([double]$pi.CommitLimit.ToInt64() * $page) / 1MB
    $physAvailMB = ([double]$pi.PhysicalAvailable.ToInt64() * $page) / 1MB
    $physTotalMB = ([double]$pi.PhysicalTotal.ToInt64() * $page) / 1MB
    return [PSCustomObject]@{
        CommitTotalMB     = [Math]::Round($commitTotalMB, 1)
        CommitLimitMB     = [Math]::Round($commitLimitMB, 1)
        CommitHeadroomMB  = [Math]::Round(($commitLimitMB - $commitTotalMB), 1)
        PhysicalAvailMB   = [Math]::Round($physAvailMB, 1)
        PhysicalTotalMB   = [Math]::Round($physTotalMB, 1)
    }
}

function Get-ProcessGpuMemoryMB {
    param([int]$TargetPid)
    $localMB = $null
    $nonLocalMB = $null
    try {
        $counters = Get-Counter -Counter @(
            '\GPU Process Memory(*)\Local Usage',
            '\GPU Process Memory(*)\Non Local Usage'
        ) -ErrorAction Stop
        $pidTag = "pid_${TargetPid}_"
        $lSum = 0.0
        $nlSum = 0.0
        $found = $false
        foreach ($s in $counters.CounterSamples) {
            if ($s.InstanceName -like "*$pidTag*") {
                $found = $true
                if ($s.Path -like '*\local usage') {
                    $lSum += [double]$s.CookedValue
                } elseif ($s.Path -like '*\non local usage') {
                    $nlSum += [double]$s.CookedValue
                }
            }
        }
        if ($found) {
            $localMB = [Math]::Round($lSum / 1MB, 1)
            $nonLocalMB = [Math]::Round($nlSum / 1MB, 1)
        }
    } catch {
        # GPU perf counters may be unavailable in some sessions; keep null gracefully.
    }
    return [PSCustomObject]@{
        GpuLocalMB    = $localMB
        GpuNonLocalMB = $nonLocalMB
    }
}

function Format-CommandArgs {
    param([string[]]$ArgList)
    $escaped = foreach ($a in $ArgList) {
        if ($a -match '[\s"]') {
            '"' + ($a -replace '(\\*)"', '$1$1\"') + '"'
        } else {
            $a
        }
    }
    return ($escaped -join ' ')
}

# Safety gate: never allow accidental or unapproved proprietary game launch.
$isSynthetic = ($PSCmdlet.ParameterSetName -eq 'Synthetic')
if (-not $isSynthetic) {
    if (-not $AuthorizeGameLaunch) {
        throw "Refusing to launch a title executable without explicit -AuthorizeGameLaunch authorization."
    }
    if (-not (Test-Path -LiteralPath $TitleExe)) {
        throw "Title executable not found: $TitleExe"
    }
}

if ($MinCommitHeadroomGB -lt 1.0) {
    throw "MinCommitHeadroomGB must be at least 1.0 GB (default 5.0 GB)."
}

# 1. Verify Git state.
$branch = (& git -C $RepoRoot rev-parse --abbrev-ref HEAD).Trim()
$commit = (& git -C $RepoRoot rev-parse HEAD).Trim()
$shortCommit = (& git -C $RepoRoot rev-parse --short HEAD).Trim()
$dirty = (& git -C $RepoRoot status --short)
$isDirty = -not [string]::IsNullOrWhiteSpace(($dirty -join ''))

$stamp = (Get-Date).ToString('yyyyMMdd-HHmmss')
$modeTag = if ($isSynthetic) { 'synthetic' } else { 'hw' }
$runDir = Join-Path $ResultsRoot ("diag-$modeTag-$stamp")
New-Item -ItemType Directory -Path $runDir -Force | Out-Null

$stdoutPath = Join-Path $runDir 'stdout.log'
$stderrPath = Join-Path $runDir 'stderr.log'
$samplesPath = Join-Path $runDir 'samples.csv'
$reportPath = Join-Path $runDir 'report.md'
$summaryPath = Join-Path $runDir 'summary.json'

Write-Host "=== HUST-VIctory Diagnostic Harness ==="
Write-Host "Mode:      $modeTag"
Write-Host "Branch:    $branch ($shortCommit)$(if ($isDirty) { ' [DIRTY]' } else { '' })"
Write-Host "Output:    $runDir"
Write-Host "Limits:    MaxDuration=${MaxDurationSeconds}s, MinCommitHeadroom=${MinCommitHeadroomGB} GB"

# 2. Build diagnostics executable unless skipped.
$diagExe = Join-Path $RepoRoot 'target\diag\victory.exe'
$buildDurationSec = 0.0
if (-not $SkipBuild) {
    Write-Host "Building diagnostics executable (cargo build --profile diag -p victory --features diagnostics)..."
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath $cargoBin) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
    $swBuild = [System.Diagnostics.Stopwatch]::StartNew()
    Push-Location $RepoRoot
    try {
        & cargo build --profile diag -p victory --features diagnostics
        if ($LASTEXITCODE -ne 0) {
            throw "Diagnostics build failed with exit code $LASTEXITCODE."
        }
    } finally {
        Pop-Location
        $swBuild.Stop()
        $buildDurationSec = [Math]::Round($swBuild.Elapsed.TotalSeconds, 2)
    }
}

if (-not $isSynthetic -and -not (Test-Path -LiteralPath $diagExe)) {
    throw "Diagnostics executable not found at $diagExe."
}

# 3. Pre-launch system memory baseline check.
$baselineMem = Get-SystemMemorySnapshot
$minHeadroomMB = $MinCommitHeadroomGB * 1024.0
Write-Host ("Baseline:  Commit {0}/{1} MB (headroom {2} MB), PhysAvail {3}/{4} MB" -f `
    $baselineMem.CommitTotalMB, $baselineMem.CommitLimitMB, $baselineMem.CommitHeadroomMB, `
    $baselineMem.PhysicalAvailMB, $baselineMem.PhysicalTotalMB)

if ($baselineMem.CommitHeadroomMB -lt $minHeadroomMB) {
    throw ("Refusing launch: preexisting system commit headroom ({0} MB) is already below the {1} GB safety floor. Close heavy background applications first." -f `
        $baselineMem.CommitHeadroomMB, $MinCommitHeadroomGB)
}

# 4. Prepare child process environment and command line.
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $false
$psi.WorkingDirectory = $RepoRoot

if ($isSynthetic) {
    $psi.FileName = $SyntheticCommand
    $psi.Arguments = Format-CommandArgs -ArgList $SyntheticArgs
} else {
    $psi.FileName = $diagExe
    $cmdArgs = @('run', $TitleExe, '--execute', '--console', $Console)
    if ($TitleArgs.Count -gt 0) {
        $cmdArgs += '--'
        $cmdArgs += $TitleArgs
    }
    $psi.Arguments = Format-CommandArgs -ArgList $cmdArgs
}

# Clean controlled environment: commit guard MUST remain enabled.
$psi.EnvironmentVariables.Remove('VICTORY_MEMORY_GUARD')
$psi.EnvironmentVariables['VICTORY_LOG'] = $VictoryLog
$psi.EnvironmentVariables['VICTORY_MEMORY_REPORT_DIR'] = $runDir

if ($EnableDebugLayer) {
    $psi.EnvironmentVariables['VICTORY_D3D12_DEBUG'] = '1'
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_D3D12_DEBUG')
}

if ($EnableDred) {
    $psi.EnvironmentVariables['VICTORY_D3D12_DRED'] = '1'
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_D3D12_DRED')
}

if ($EnableCapture) {
    $framesDir = Join-Path $runDir 'frames'
    New-Item -ItemType Directory -Path $framesDir -Force | Out-Null
    $psi.EnvironmentVariables['VICTORY_CAPTURE'] = $framesDir
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_CAPTURE')
}

if ($DisableLazyTextures) {
    $psi.EnvironmentVariables['VICTORY_LAZY_TEXTURES'] = '0'
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_LAZY_TEXTURES')
}

# 5. Launch and monitor process.
$runStartUtc = [DateTime]::UtcNow
$proc = New-Object System.Diagnostics.Process
$proc.StartInfo = $psi

$stdoutWriter = [System.IO.StreamWriter]::new($stdoutPath, $false, [System.Text.Encoding]::UTF8)
$stderrWriter = [System.IO.StreamWriter]::new($stderrPath, $false, [System.Text.Encoding]::UTF8)

$samples = New-Object System.Collections.Generic.List[object]
$harnessStopReason = 'ProcessExited'
$exitCode = $null
$elapsedSec = 0.0

$peakPrivateMB = 0.0
$peakWorkingSetMB = 0.0
$peakCommitTotalMB = $baselineMem.CommitTotalMB
$minObservedHeadroomMB = $baselineMem.CommitHeadroomMB
$peakGpuLocalMB = $null
$peakGpuNonLocalMB = $null

try {
    [void]$proc.Start()
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $swRun = [System.Diagnostics.Stopwatch]::StartNew()

    while (-not $proc.HasExited) {
        $sysMem = Get-SystemMemorySnapshot
        $privMB = 0.0
        $wsMB = 0.0
        try {
            $proc.Refresh()
            if (-not $proc.HasExited) {
                $privMB = [Math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
                $wsMB = [Math]::Round($proc.WorkingSet64 / 1MB, 1)
            }
        } catch {
            # Process may exit mid-refresh.
        }
        $gpuMem = Get-ProcessGpuMemoryMB -TargetPid $proc.Id

        if ($privMB -gt $peakPrivateMB) { $peakPrivateMB = $privMB }
        if ($wsMB -gt $peakWorkingSetMB) { $peakWorkingSetMB = $wsMB }
        if ($sysMem.CommitTotalMB -gt $peakCommitTotalMB) { $peakCommitTotalMB = $sysMem.CommitTotalMB }
        if ($sysMem.CommitHeadroomMB -lt $minObservedHeadroomMB) { $minObservedHeadroomMB = $sysMem.CommitHeadroomMB }
        if ($null -ne $gpuMem.GpuLocalMB -and ($null -eq $peakGpuLocalMB -or $gpuMem.GpuLocalMB -gt $peakGpuLocalMB)) {
            $peakGpuLocalMB = $gpuMem.GpuLocalMB
        }
        if ($null -ne $gpuMem.GpuNonLocalMB -and ($null -eq $peakGpuNonLocalMB -or $gpuMem.GpuNonLocalMB -gt $peakGpuNonLocalMB)) {
            $peakGpuNonLocalMB = $gpuMem.GpuNonLocalMB
        }

        $samples.Add([PSCustomObject]@{
            TimestampUtc      = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
            ElapsedSec        = [Math]::Round($swRun.Elapsed.TotalSeconds, 1)
            PrivateMB         = $privMB
            WorkingSetMB      = $wsMB
            CommitTotalMB     = $sysMem.CommitTotalMB
            CommitLimitMB     = $sysMem.CommitLimitMB
            CommitHeadroomMB  = $sysMem.CommitHeadroomMB
            PhysicalAvailMB   = $sysMem.PhysicalAvailMB
            GpuLocalMB        = $gpuMem.GpuLocalMB
            GpuNonLocalMB     = $gpuMem.GpuNonLocalMB
        })

        if ($sysMem.CommitHeadroomMB -lt $minHeadroomMB) {
            $harnessStopReason = 'HarnessCommitHeadroomGuard'
            Write-Warning ("Stopping process: system commit headroom ({0} MB) fell below {1} GB floor." -f `
                $sysMem.CommitHeadroomMB, $MinCommitHeadroomGB)
            try { $proc.Kill() } catch {}
            break
        }

        if ($swRun.Elapsed.TotalSeconds -ge $MaxDurationSeconds) {
            $harnessStopReason = 'MaxDurationReached'
            Write-Host "Stopping process: max duration (${MaxDurationSeconds}s) reached."
            try { $proc.Kill() } catch {}
            break
        }

        Start-Sleep -Seconds $SampleIntervalSeconds
    }

    $proc.WaitForExit(15000) | Out-Null
    $swRun.Stop()
    $elapsedSec = [Math]::Round($swRun.Elapsed.TotalSeconds, 2)
    $exitCode = if ($proc.HasExited) { $proc.ExitCode } else { -1 }

    $stdoutText = $outTask.GetAwaiter().GetResult()
    $stderrText = $errTask.GetAwaiter().GetResult()
    $stdoutWriter.Write($stdoutText)
    $stderrWriter.Write($stderrText)
} finally {
    $stdoutWriter.Dispose()
    $stderrWriter.Dispose()
    if ($null -ne $proc) { $proc.Dispose() }
}

if ($samples.Count -gt 0) {
    $samples | Export-Csv -LiteralPath $samplesPath -NoTypeInformation
}

# 6. Collect crash logs written/updated during this run (outside repo).
$collectedCrashes = New-Object System.Collections.Generic.List[string]
$victoryDataRoot = Join-Path $env:LOCALAPPDATA 'victory\data'
if (Test-Path -LiteralPath $victoryDataRoot) {
    Get-ChildItem -Path $victoryDataRoot -Filter 'crashes.log' -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
        if ($_.LastWriteTimeUtc -ge $runStartUtc.AddSeconds(-5)) {
            $destName = ($_.Directory.Name + '-crashes.log')
            $destPath = Join-Path $runDir $destName
            Copy-Item -LiteralPath $_.FullName -Destination $destPath -Force
            $collectedCrashes.Add($destPath)
        }
    }
}

$memLogs = @(Get-ChildItem -Path $runDir -Filter 'memory-*.log' -ErrorAction SilentlyContinue | Sort-Object Name)

# 7. Classify outcome (distinguish memory-guard stop vs D3D12 device removal vs timeout vs crash).
$combinedText = ""
if (Test-Path -LiteralPath $stdoutPath) { $combinedText += (Get-Content -LiteralPath $stdoutPath -Raw -ErrorAction SilentlyContinue) + "`n" }
if (Test-Path -LiteralPath $stderrPath) { $combinedText += (Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue) + "`n" }
foreach ($ml in $memLogs) {
    $combinedText += (Get-Content -LiteralPath $ml.FullName -Raw -ErrorAction SilentlyContinue) + "`n"
}
foreach ($cl in $collectedCrashes) {
    $combinedText += (Get-Content -LiteralPath $cl -Raw -ErrorAction SilentlyContinue) + "`n"
}

$classification = 'CleanExit'
$notifyStatus = 'Success'

if ($harnessStopReason -eq 'HarnessCommitHeadroomGuard') {
    $classification = 'HarnessCommitHeadroomGuard'
    $notifyStatus = 'Warning'
} elseif ($combinedText -match '(?i)(DXGI_ERROR_DEVICE_HUNG|DXGI_ERROR_DEVICE_REMOVED|DXGI_ERROR_DEVICE_RESET|0x887A0005|0x887A0006|0x887A0007|host GPU device removed)') {
    $classification = 'D3D12DeviceRemoval'
    $notifyStatus = 'Failure'
} elseif ($combinedText -match '(?i)(commit guard|stopping before.*commit|out of commit|system commit headroom)') {
    $classification = 'VictoryCommitGuardStop'
    $notifyStatus = 'Warning'
} elseif ($harnessStopReason -eq 'MaxDurationReached') {
    $classification = 'MaxDurationReached'
    $notifyStatus = 'Success'
} elseif ($exitCode -ne 0) {
    $classification = 'NonZeroExitOrCrash'
    $notifyStatus = 'Failure'
}

# 8. Write JSON summary and Markdown report.
$summary = [ordered]@{
    TimestampUtc          = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
    Mode                  = $modeTag
    Branch                = $branch
    Commit                = $commit
    ShortCommit           = $shortCommit
    DirtyWorkingTree      = $isDirty
    BuildDurationSec      = $buildDurationSec
    ElapsedSec            = $elapsedSec
    ExitCode              = $exitCode
    HarnessStopReason     = $harnessStopReason
    Classification        = $classification
    BaselineCommitMB      = $baselineMem.CommitTotalMB
    BaselineCommitLimitMB = $baselineMem.CommitLimitMB
    BaselineHeadroomMB    = $baselineMem.CommitHeadroomMB
    PeakPrivateMB         = $peakPrivateMB
    PeakWorkingSetMB      = $peakWorkingSetMB
    PeakCommitTotalMB     = $peakCommitTotalMB
    MinObservedHeadroomMB = $minObservedHeadroomMB
    PeakGpuLocalMB        = $peakGpuLocalMB
    PeakGpuNonLocalMB     = $peakGpuNonLocalMB
    SampleCount           = $samples.Count
    MemoryLogs            = @($memLogs | ForEach-Object { $_.FullName })
    CrashLogs             = @($collectedCrashes)
    CaptureEnabled        = [bool]$EnableCapture
    DebugLayerEnabled     = [bool]$EnableDebugLayer
    DredEnabled           = [bool]$EnableDred
    LazyTexturesDisabled  = [bool]$DisableLazyTextures
    RunDirectory          = $runDir
}

$summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $summaryPath -Encoding UTF8

$gpuLocalStr = if ($null -ne $peakGpuLocalMB) { "$peakGpuLocalMB MB" } else { 'N/A (counter unavailable)' }
$gpuNonLocalStr = if ($null -ne $peakGpuNonLocalMB) { "$peakGpuNonLocalMB MB" } else { 'N/A (counter unavailable)' }

$mdLines = @(
    "# HUST-VIctory Diagnostic Report ($modeTag)",
    "",
    "- **Timestamp (UTC)**: $($summary.TimestampUtc)",
    "- **Branch / Commit**: ``$branch`` @ ``$shortCommit`` (full: ``$commit``)$(if ($isDirty) { ' [DIRTY]' } else { '' })",
    "- **Classification**: **$classification** (harness stop: ``$harnessStopReason``, exit code: ``$exitCode``)",
    "- **Duration**: ${elapsedSec}s (max ${MaxDurationSeconds}s; build ${buildDurationSec}s)",
    "- **Switches**: Capture=$EnableCapture, DebugLayer=$EnableDebugLayer, DRED=$EnableDred, LazyTextures=$(-not $DisableLazyTextures), CommitGuard=ON",
    "",
    "## Memory & GPU Summary",
    "",
    "| Metric | Value |",
    "|---|---:|",
    "| Baseline system commit / limit | $($baselineMem.CommitTotalMB) / $($baselineMem.CommitLimitMB) MB |",
    "| Baseline commit headroom | $($baselineMem.CommitHeadroomMB) MB |",
    "| Peak system commit | $peakCommitTotalMB MB |",
    "| Minimum observed commit headroom | $minObservedHeadroomMB MB (floor: $minHeadroomMB MB) |",
    "| Peak process private bytes | $peakPrivateMB MB |",
    "| Peak process working set | $peakWorkingSetMB MB |",
    "| Peak GPU local usage (process) | $gpuLocalStr |",
    "| Peak GPU non-local usage (process) | $gpuNonLocalStr |",
    "| Telemetry samples recorded | $($samples.Count) |",
    "",
    "## Artifacts",
    "",
    "- Stdout: ``$stdoutPath``",
    "- Stderr: ``$stderrPath``",
    "- Samples CSV: ``$samplesPath``",
    "- Summary JSON: ``$summaryPath``",
    "- VIctory memory logs: $(if ($memLogs.Count -gt 0) { ($memLogs | ForEach-Object { '``' + $_.FullName + '``' }) -join ', ' } else { 'none' })",
    "- Collected crash logs: $(if ($collectedCrashes.Count -gt 0) { ($collectedCrashes | ForEach-Object { '``' + $_ + '``' }) -join ', ' } else { 'none' })"
)
$mdLines -join "`r`n" | Set-Content -LiteralPath $reportPath -Encoding UTF8

Write-Host "=== Diagnostic Complete ==="
Write-Host "Classification: $classification (exit=$exitCode, elapsed=${elapsedSec}s)"
Write-Host "Peak Private:   $peakPrivateMB MB | Min Commit Headroom: $minObservedHeadroomMB MB"
Write-Host "Report:         $reportPath"

# 9. Notify Discord if configured and enabled.
if (-not $NoDiscord) {
    $discordScript = Join-Path $RepoRoot 'tools\discord\Send-DiscordNotification.ps1'
    if (Test-Path -LiteralPath $discordScript) {
        $desc = "Diagnostic ($modeTag) finished: $classification (exit=$exitCode, ${elapsedSec}s). Peak private=${peakPrivateMB} MB, min commit headroom=${minObservedHeadroomMB} MB. Report: $reportPath"
        $next = if ($classification -eq 'D3D12DeviceRemoval') {
            "Inspect DRED/stderr/crash logs in $runDir."
        } elseif ($classification -like '*Commit*') {
            "Inspect memory-*.log commit breakdown in $runDir."
        } else {
            "Review report.md and samples.csv in $runDir."
        }
        & $discordScript `
            -Category 'Diagnostic' `
            -Status $notifyStatus `
            -Description $desc `
            -NextAction $next `
            -Branch $branch `
            -Commit $shortCommit `
            -RepoRoot $RepoRoot | Out-Null
    }
}

[PSCustomObject]$summary
