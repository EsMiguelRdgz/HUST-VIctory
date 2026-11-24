#Requires -Version 5.1
<#
.SYNOPSIS
    Controlled, safety-guarded diagnostic harness for HUST-VIctory.
.DESCRIPTION
    Verifies Git branch/commit, records persistent hardware launch history
    outside the repo (with no fixed daily launch quota by default; optional
    -MaxLaunchesPer24h if explicitly set > 0), verifies NVIDIA GPU adapter
    health and recent display-driver TDR stability, enforces single-instance
    exclusivity, >= 60 s recovery interval, >= 10 GB pre-launch system commit
    headroom, builds the diagnostics executable, runs either a synthetic test
    command (-SyntheticCommand) or an authorized title executable
    (`victory run <exe> --execute` with -AuthorizeGameLaunch), enforces max
    duration (default 90 s) and >= 5 GB runtime commit headroom cutoff,
    classifies outcomes, writes timestamped reports under G:\VIctory-Results,
    and sends consolidated Discord notifications.

    Synthetic runs (-SyntheticCommand) are hardware runs unless
    -SyntheticCpuOnly is passed: they get the same preflight (NVIDIA health,
    no Event 4101 in 3 min, single instance including cargo test binaries,
    >= 10 GB headroom, recovery interval) and launch history as a title
    launch. -SyntheticCpuOnly forces the software adapter (VICTORY_WARP=1,
    VICTORY_D3D12_WARP=1) instead. On a timeout or guard stop the whole
    process tree is terminated (cargo and every test binary it started),
    and anything still running after the child exits is reported and ended.
    Synthetic outcomes report cargo test counts, every skipped/unsupported
    dependency line (CleanExitWithSkips instead of a silent CleanExit), and
    the D3D12 debug-layer state per crate from the probe tests
    (VICTORY_TEST_PROBE_FILE; DebugLayerNotVerified when requested but not
    shown active).

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

    # A synthetic command that must not touch the GPU: the child gets
    # VICTORY_WARP=1 and VICTORY_D3D12_WARP=1 (software adapter only) and the
    # hardware preflight is skipped. Without it, a synthetic run is a
    # hardware run and gets every preflight check a title launch gets.
    [Parameter(ParameterSetName = 'Synthetic')]
    [switch]$SyntheticCpuOnly,

    [string]$RepoRoot = 'G:\VIctory',
    [string]$ResultsRoot = 'G:\VIctory-Results',
    [int]$MaxDurationSeconds = 90,
    [double]$MinPreLaunchHeadroomGB = 10.0,
    [double]$MinCommitHeadroomGB = 5.0,
    [int]$RecoveryIntervalSeconds = 60,
    [int]$MaxLaunchesPer24h = 0,
    [int]$SampleIntervalSeconds = 2,
    [string]$VictoryLog = 'info',
    [string]$IsolatedCacheDir = '',
    [string]$TestLabel = '',
    [string]$ScriptedKeys = '',
    [string]$CaptureFrames = '',
    [switch]$EnableCapture,
    [switch]$EnableDebugLayer,
    [switch]$EnableDred,
    [switch]$DisableLazyTextures,
    [switch]$SyncPipelines,
    [switch]$DisablePipelineCache,
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

function Read-LaunchHistory {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        return @()
    }
    try {
        $raw = Get-Content -LiteralPath $Path -Raw
        if ([string]::IsNullOrWhiteSpace($raw)) { return @() }
        $parsed = $raw | ConvertFrom-Json
        if ($parsed -is [System.Array]) { return @($parsed) }
        return @($parsed)
    } catch {
        return @()
    }
}

function Save-LaunchHistory {
    param([string]$Path, [object[]]$Entries)
    $dir = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
    }
    ConvertTo-Json -InputObject @($Entries) -Depth 5 | Set-Content -LiteralPath $Path -Encoding UTF8
}

. (Join-Path $PSScriptRoot 'VictoryDiagnosticLib.ps1')

# Safety gates: never allow accidental or unapproved proprietary game launch.
$isSynthetic = ($PSCmdlet.ParameterSetName -eq 'Synthetic')
# A hardware run is a title launch or any synthetic run not declared CPU-only:
# both get the same preflight, floors, recovery interval and launch history.
$hardwareRun = -not $isSynthetic -or -not $SyntheticCpuOnly
if ($MaxLaunchesPer24h -gt 4) {
    throw "Hard safety limit: MaxLaunchesPer24h cannot exceed 4."
}
if ($MinCommitHeadroomGB -lt 5.0 -and $hardwareRun) {
    throw "Hard safety limit: MinCommitHeadroomGB must be at least 5.0 GB for hardware launches."
}
if ($MinPreLaunchHeadroomGB -lt 10.0 -and $hardwareRun) {
    throw "Hard safety limit: MinPreLaunchHeadroomGB must be at least 10.0 GB for hardware launches."
}
# Ignored tests include known-defect regressions (WARP only); they never run
# on the hardware adapter through this harness.
if ($isSynthetic -and $hardwareRun -and @($SyntheticArgs | Where-Object { $_ -in @('--ignored', '--include-ignored') }).Count -gt 0) {
    throw "Refusing launch: --ignored/--include-ignored on the hardware adapter would run known-defect tests on the GPU; use -SyntheticCpuOnly."
}

$launchHistoryPath = Join-Path (Join-Path $env:LOCALAPPDATA 'HUST-VIctory') 'hw-launch-history.json'
$allHistory = @()
$recentLaunches = @()
$launchNumber24h = 0
$launchNumberTotal = 0
$preflightReport = 'skipped (CPU-only synthetic run: software adapter forced)'

if ($hardwareRun) {
    if (-not $isSynthetic) {
        if (-not $AuthorizeGameLaunch) {
            throw "Refusing to launch a title executable without explicit -AuthorizeGameLaunch authorization."
        }
        if (-not (Test-Path -LiteralPath $TitleExe)) {
            throw "Title executable not found: $TitleExe"
        }
    }

    # Single-instance exclusivity check (sequential execution only): the
    # launcher, the title, and this repository's cargo test binaries.
    $existing = @(Get-Process -Name 'victory', 'game_scarlett_master' -ErrorAction SilentlyContinue) +
        @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -like 'victory_*-*' })
    # NVIDIA adapter health and recent OS TDR resets.
    $gpuAdapters = @(Get-CimInstance -ClassName Win32_VideoController -ErrorAction SilentlyContinue)
    $recentTdr = @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = 'Display'; Id = 4101; StartTime = (Get-Date).AddMinutes(-3) } -ErrorAction SilentlyContinue)

    # Persistent launch history & recovery interval (no daily launch cap unless MaxLaunchesPer24h > 0).
    $nowUtc = [DateTime]::UtcNow
    $cutoffUtc = $nowUtc.AddHours(-24)
    $allHistory = @(Read-LaunchHistory -Path $launchHistoryPath)
    $recentLaunches = @($allHistory | Where-Object {
        try { [DateTime]::Parse($_.StartUtc).ToUniversalTime() -ge $cutoffUtc } catch { $false }
    })
    if ($MaxLaunchesPer24h -gt 0 -and $recentLaunches.Count -ge $MaxLaunchesPer24h) {
        throw ("Refusing launch: optional 24-hour hardware launch cap reached ({0}/{1})." -f $recentLaunches.Count, $MaxLaunchesPer24h)
    }
    $lastLaunch = $allHistory | Sort-Object { try { [DateTime]::Parse($_.EndUtc).ToUniversalTime() } catch { [DateTime]::MinValue } } | Select-Object -Last 1
    $lastEnd = $null
    if ($null -ne $lastLaunch) {
        try { $lastEnd = [DateTime]::Parse($lastLaunch.EndUtc).ToUniversalTime() } catch { $lastEnd = $null }
    }

    $preflight = Get-VictoryPreflightProblems `
        -Adapters $gpuAdapters `
        -RecentTdrEvents $recentTdr `
        -RunningProcessNames @($existing | ForEach-Object { $_.ProcessName }) `
        -CommitHeadroomMB (Get-SystemMemorySnapshot).CommitHeadroomMB `
        -MinPreLaunchHeadroomGB $MinPreLaunchHeadroomGB `
        -LastLaunchEndUtc $lastEnd `
        -NowUtc $nowUtc `
        -RecoveryIntervalSeconds $RecoveryIntervalSeconds
    if (@($preflight.Problems).Count -gt 0) {
        throw ("Refusing launch: " + (@($preflight.Problems) -join '; ') + '.')
    }
    if ($RecoveryIntervalSeconds -gt 0 -and $preflight.WaitSeconds -gt 0) {
        Write-Host "Enforcing ${RecoveryIntervalSeconds}s GPU recovery interval: waiting $($preflight.WaitSeconds)s..."
        Start-Sleep -Seconds $preflight.WaitSeconds
    }
    $nvName = @($gpuAdapters | Where-Object { $_.Name -like '*NVIDIA*' } | ForEach-Object { "$($_.Name) (driver $($_.DriverVersion))" }) | Select-Object -First 1
    $preflightReport = "passed: $nvName healthy; no Event 4101 in 3 min; no running VIctory/title/test process; commit headroom >= ${MinPreLaunchHeadroomGB} GB; recovery interval ${RecoveryIntervalSeconds}s"
    $launchNumber24h = $recentLaunches.Count + 1
    $launchNumberTotal = $allHistory.Count + 1
}

# 1. Verify Git state.
$branch = (& git -C $RepoRoot rev-parse --abbrev-ref HEAD).Trim()
$commit = (& git -C $RepoRoot rev-parse HEAD).Trim()
$shortCommit = (& git -C $RepoRoot rev-parse --short HEAD).Trim()
$dirty = (& git -C $RepoRoot status --short)
$isDirty = -not [string]::IsNullOrWhiteSpace(($dirty -join ''))

$stamp = (Get-Date).ToString('yyyyMMdd-HHmmss')
$modeTag = if (-not $isSynthetic) { 'hw' } elseif ($hardwareRun) { 'synthetic-hw' } else { 'synthetic-cpu' }
$runDir = Join-Path $ResultsRoot ("diag-$modeTag-$stamp")
New-Item -ItemType Directory -Path $runDir -Force | Out-Null

$stdoutPath = Join-Path $runDir 'stdout.log'
$stderrPath = Join-Path $runDir 'stderr.log'
$samplesPath = Join-Path $runDir 'samples.csv'
$reportPath = Join-Path $runDir 'report.md'
$summaryPath = Join-Path $runDir 'summary.json'

Write-Host "=== HUST-VIctory Diagnostic Harness ==="
$launchBadge = if ($hardwareRun) {
    if ($MaxLaunchesPer24h -gt 0) { " (launch #$launchNumberTotal total, #$launchNumber24h/$MaxLaunchesPer24h in 24h)" }
    else { " (launch #$launchNumberTotal total, #$launchNumber24h in 24h, unlimited)" }
} else { '' }
Write-Host "Mode:      $modeTag$launchBadge"
Write-Host "Branch:    $branch ($shortCommit)$(if ($isDirty) { ' [DIRTY]' } else { '' })"
Write-Host "Output:    $runDir"
Write-Host "Preflight: $preflightReport"
Write-Host "Limits:    MaxDuration=${MaxDurationSeconds}s, PreLaunchHeadroom>=${MinPreLaunchHeadroomGB} GB, RuntimeHeadroom>=${MinCommitHeadroomGB} GB"

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
$minPreLaunchMB = $MinPreLaunchHeadroomGB * 1024.0
$minHeadroomMB = $MinCommitHeadroomGB * 1024.0
Write-Host ("Baseline:  Commit {0}/{1} MB (headroom {2} MB), PhysAvail {3}/{4} MB" -f `
    $baselineMem.CommitTotalMB, $baselineMem.CommitLimitMB, $baselineMem.CommitHeadroomMB, `
    $baselineMem.PhysicalAvailMB, $baselineMem.PhysicalTotalMB)

$requiredStartMB = if ($hardwareRun) { $minPreLaunchMB } else { $minHeadroomMB }
if ($baselineMem.CommitHeadroomMB -lt $requiredStartMB) {
    throw ("Refusing launch: preexisting system commit headroom ({0} MB) is below the {1} MB pre-launch safety floor." -f `
        $baselineMem.CommitHeadroomMB, $requiredStartMB)
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

if ($EnableDebugLayer) { $psi.EnvironmentVariables['VICTORY_D3D12_DEBUG'] = '1' } else { $psi.EnvironmentVariables.Remove('VICTORY_D3D12_DEBUG') }
if ($EnableDred) { $psi.EnvironmentVariables['VICTORY_D3D12_DRED'] = '1' } else { $psi.EnvironmentVariables.Remove('VICTORY_D3D12_DRED') }
if ($EnableCapture) {
    $framesDir = Join-Path $runDir 'frames'
    New-Item -ItemType Directory -Path $framesDir -Force | Out-Null
    $psi.EnvironmentVariables['VICTORY_CAPTURE'] = $framesDir
    if (-not [string]::IsNullOrWhiteSpace($CaptureFrames)) {
        $psi.EnvironmentVariables['VICTORY_CAPTURE_FRAMES'] = $CaptureFrames
    } else {
        $psi.EnvironmentVariables.Remove('VICTORY_CAPTURE_FRAMES')
    }
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_CAPTURE')
    $psi.EnvironmentVariables.Remove('VICTORY_CAPTURE_FRAMES')
}
if (-not [string]::IsNullOrWhiteSpace($ScriptedKeys)) {
    $psi.EnvironmentVariables['VICTORY_KEYS'] = $ScriptedKeys
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_KEYS')
}
if ($DisableLazyTextures) { $psi.EnvironmentVariables['VICTORY_LAZY_TEXTURES'] = '0' } else { $psi.EnvironmentVariables.Remove('VICTORY_LAZY_TEXTURES') }
if ($SyncPipelines) { $psi.EnvironmentVariables['VICTORY_SYNC_PIPELINES'] = '1' } else { $psi.EnvironmentVariables.Remove('VICTORY_SYNC_PIPELINES') }
if ($DisablePipelineCache) { $psi.EnvironmentVariables['VICTORY_PIPELINE_CACHE'] = '0' } else { $psi.EnvironmentVariables.Remove('VICTORY_PIPELINE_CACHE') }
if (-not [string]::IsNullOrWhiteSpace($IsolatedCacheDir)) {
    New-Item -ItemType Directory -Path $IsolatedCacheDir -Force | Out-Null
    $psi.EnvironmentVariables['VICTORY_CACHE_DIR'] = $IsolatedCacheDir
} else {
    $psi.EnvironmentVariables.Remove('VICTORY_CACHE_DIR')
}
# Adapter choice is explicit: a CPU-only run gets the software adapter
# (WARP) in every crate; a hardware run gets the hardware adapter.
if ($hardwareRun) {
    $psi.EnvironmentVariables.Remove('VICTORY_WARP')
    $psi.EnvironmentVariables.Remove('VICTORY_D3D12_WARP')
} else {
    $psi.EnvironmentVariables['VICTORY_WARP'] = '1'
    $psi.EnvironmentVariables['VICTORY_D3D12_WARP'] = '1'
}
# The D3D12 probe tests append what they observed (adapter, debug layer).
$probePath = Join-Path $runDir 'test-probe.jsonl'
$psi.EnvironmentVariables['VICTORY_TEST_PROBE_FILE'] = $probePath

$discordScript = Join-Path $RepoRoot 'tools\discord\Send-DiscordNotification.ps1'
$switchSummary = "Debug=$EnableDebugLayer, DRED=$EnableDred, Capture=$EnableCapture, Lazy=$(-not $DisableLazyTextures), SyncPipe=$SyncPipelines, PipeCache=$(-not $DisablePipelineCache)"

# Record pre-launch byte offsets of existing crashes.log files so outcome
# classification inspects only entries appended during THIS run.
$preCrashLengths = @{}
$victoryDataRoot = Join-Path $env:LOCALAPPDATA 'victory\data'
if (Test-Path -LiteralPath $victoryDataRoot) {
    Get-ChildItem -Path $victoryDataRoot -Filter 'crashes.log' -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
        $preCrashLengths[$_.FullName] = $_.Length
    }
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

# Every process the child starts (cargo -> test binaries -> ...) is recorded
# while it runs, so a timeout or guard stop ends the whole tree, and
# nothing it started may outlive the run.
$rootPid = $null
$knownTree = @{}
$treeTerminated = @()
$treeRemaining = @()
$stragglers = @()

try {
    [void]$proc.Start()
    $rootPid = $proc.Id
    $rootCim = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $rootPid" -ErrorAction SilentlyContinue
    $rootCreated = if ($null -ne $rootCim) { $rootCim.CreationDate } else { (Get-Date).AddSeconds(-5) }
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $swRun = [System.Diagnostics.Stopwatch]::StartNew()

    while (-not $proc.HasExited) {
        Update-VictoryKnownTree -RootId $rootPid -Known $knownTree -RootCreationDate $rootCreated
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
            $stop = Stop-VictoryProcessTree -RootId $rootPid -Known $knownTree -RootCreationDate $rootCreated
            $treeTerminated = @($stop.Terminated); $treeRemaining = @($stop.Remaining)
            break
        }

        if ($swRun.Elapsed.TotalSeconds -ge $MaxDurationSeconds) {
            $harnessStopReason = 'MaxDurationReached'
            Write-Host "Stopping process: max duration (${MaxDurationSeconds}s) reached; terminating the process tree."
            $stop = Stop-VictoryProcessTree -RootId $rootPid -Known $knownTree -RootCreationDate $rootCreated
            $treeTerminated = @($stop.Terminated); $treeRemaining = @($stop.Remaining)
            break
        }

        Start-Sleep -Seconds $SampleIntervalSeconds
    }

    $proc.WaitForExit(15000) | Out-Null
    $swRun.Stop()
    $elapsedSec = [Math]::Round($swRun.Elapsed.TotalSeconds, 2)
    $exitCode = if ($proc.HasExited) { $proc.ExitCode } else { -1 }
    $rootExitedAt = Get-Date

    # Anything the child started that outlived it is a straggler: report it
    # and end it (never leave a GPU workload running after the harness).
    $after = Stop-VictoryProcessTree -RootId $rootPid -Known $knownTree -RootCreationDate $rootCreated -RootExitedAt $rootExitedAt -WaitMilliseconds 5000
    $stragglers = @($after.Terminated | Where-Object { $_ -ne $rootPid -and $treeTerminated -notcontains $_ })
    $treeRemaining = @($after.Remaining)

    # A surviving grandchild can hold the pipes open; never wait forever.
    $stdoutText = if ($outTask.Wait(30000)) { $outTask.Result } else { '[harness: stdout still held open after 30 s; not captured]' }
    $stderrText = if ($errTask.Wait(30000)) { $errTask.Result } else { '[harness: stderr still held open after 30 s; not captured]' }
    $stdoutWriter.Write($stdoutText)
    $stderrWriter.Write($stderrText)
} finally {
    if ($null -ne $rootPid) {
        # Also on an exception or Ctrl+C in the harness itself.
        try { [void](Stop-VictoryProcessTree -RootId $rootPid -Known $knownTree -RootCreationDate $rootCreated -RootExitedAt (Get-Date) -WaitMilliseconds 5000) } catch {}
    }
    $stdoutWriter.Dispose()
    $stderrWriter.Dispose()
    if ($null -ne $proc) { $proc.Dispose() }
}

$runEndUtc = [DateTime]::UtcNow

if ($samples.Count -gt 0) {
    $samples | Export-Csv -LiteralPath $samplesPath -NoTypeInformation
}

# 6. Collect crash logs written/updated during this run (outside repo).
$collectedCrashes = New-Object System.Collections.Generic.List[string]
$newCrashText = ""
if (Test-Path -LiteralPath $victoryDataRoot) {
    Get-ChildItem -Path $victoryDataRoot -Filter 'crashes.log' -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
        if ($_.LastWriteTimeUtc -ge $runStartUtc.AddSeconds(-5)) {
            $destName = ($_.Directory.Name + '-crashes.log')
            $destPath = Join-Path $runDir $destName
            Copy-Item -LiteralPath $_.FullName -Destination $destPath -Force
            $collectedCrashes.Add($destPath)
            $prevLen = if ($preCrashLengths.ContainsKey($_.FullName)) { [long]$preCrashLengths[$_.FullName] } else { 0L }
            if ($_.Length -gt $prevLen) {
                $fs = [System.IO.File]::OpenRead($_.FullName)
                try {
                    $fs.Seek($prevLen, [System.IO.SeekOrigin]::Begin) | Out-Null
                    $sr = New-Object System.IO.StreamReader($fs)
                    $newCrashText += $sr.ReadToEnd() + "`n"
                } finally {
                    $fs.Dispose()
                }
            }
        }
    }
}

$memLogs = @(Get-ChildItem -Path $runDir -Filter 'memory-*.log' -ErrorAction SilentlyContinue | Sort-Object Name)

# 7. Classify outcome (distinguish memory-guard stop vs D3D12 device removal/hang vs AV vs timeout vs clean exit).
$combinedText = ""
if (Test-Path -LiteralPath $stdoutPath) { $combinedText += (Get-Content -LiteralPath $stdoutPath -Raw -ErrorAction SilentlyContinue) + "`n" }
if (Test-Path -LiteralPath $stderrPath) { $combinedText += (Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue) + "`n" }
foreach ($ml in $memLogs) {
    $combinedText += (Get-Content -LiteralPath $ml.FullName -Raw -ErrorAction SilentlyContinue) + "`n"
}
$combinedText += $newCrashText

$classification = 'CleanExit'
$notifyStatus = 'Success'

if ($harnessStopReason -eq 'HarnessCommitHeadroomGuard') {
    $classification = 'HarnessCommitHeadroomGuard'
    $notifyStatus = 'Warning'
} elseif ($combinedText -match '(?i)(DXGI_ERROR_DEVICE_HUNG|0x887A0006)') {
    $classification = 'D3D12DeviceHang'
    $notifyStatus = 'Failure'
} elseif ($combinedText -match '(?i)(DXGI_ERROR_DEVICE_REMOVED|DXGI_ERROR_DEVICE_RESET|0x887A0005|0x887A0007|host GPU device removed)') {
    $classification = 'D3D12DeviceRemoval'
    $notifyStatus = 'Failure'
} elseif ($combinedText -match '(?i)(commit guard|stopping before.*commit|out of commit|system commit headroom)') {
    $classification = 'VictoryCommitGuardStop'
    $notifyStatus = 'Warning'
} elseif ($combinedText -match '(?i)(unhandled exception 0xc0000005)') {
    $classification = 'WindowsAccessViolation'
    $notifyStatus = 'Failure'
} elseif ($harnessStopReason -eq 'MaxDurationReached') {
    $classification = 'MaxDurationReached'
    $notifyStatus = 'Success'
} elseif ($exitCode -ne 0) {
    $classification = 'NonZeroExitOrCrash'
    $notifyStatus = 'Failure'
}

# 7a. Test evidence for synthetic runs: counts, failures, and every skipped
# or unavailable dependency (a test that returns early still "passes"), the
# D3D12 probe records (adapter, debug layer), and leftover processes.
$testSummary = Get-CargoTestSummary -Text $combinedText
$probe = Read-VictoryTestProbe -Path $probePath
$debugVerdict = Get-VictoryDebugLayerVerdict -Records $probe.Records -Requested ([bool]$EnableDebugLayer)
$probeSkips = @(Get-VictoryProbeSkips -Records $probe.Records)
$skipTotal = $testSummary.SkippedCount + $probeSkips.Count
$probeAdapters = @($probe.Records | Where-Object { $_.PSObject.Properties['adapter'] } |
        ForEach-Object { "$($_.crate): $($_.adapter)$(if ($_.PSObject.Properties['software'] -and $_.software) { ' (software)' } else { '' })" } | Sort-Object -Unique)
$debugProblems = @()
if ($EnableDebugLayer) {
    foreach ($p in $debugVerdict.PSObject.Properties) {
        if ($p.Value -in @('NotActive', 'NoProbe')) { $debugProblems += "$($p.Name)=$($p.Value)" }
    }
}

if (@($treeRemaining).Count -gt 0) {
    $classification = 'ProcessTreeNotTerminated'
    $notifyStatus = 'Failure'
} elseif ($testSummary.Failed -gt 0 -and $classification -in @('CleanExit', 'NonZeroExitOrCrash')) {
    $classification = 'TestFailures'
    $notifyStatus = 'Failure'
} elseif ($classification -eq 'CleanExit' -and $isSynthetic -and $testSummary.ResultLines -eq 0 -and $SyntheticCommand -match '(?i)cargo') {
    $classification = 'NoTestResults'
    $notifyStatus = 'Failure'
} elseif ($classification -eq 'CleanExit' -and $debugProblems.Count -gt 0) {
    $classification = 'DebugLayerNotVerified'
    $notifyStatus = 'Warning'
} elseif ($classification -eq 'CleanExit' -and $skipTotal -gt 0) {
    $classification = 'CleanExitWithSkips'
    $notifyStatus = 'Warning'
}

# Persist complete hardware launch history outside repository (never prunes prior records).
if ($hardwareRun) {
    $entry = [ordered]@{
        StartUtc       = $runStartUtc.ToString('yyyy-MM-ddTHH:mm:ssZ')
        EndUtc         = $runEndUtc.ToString('yyyy-MM-ddTHH:mm:ssZ')
        Mode           = $modeTag
        Commit         = $shortCommit
        Label          = $TestLabel
        Classification = $classification
        ExitCode       = $exitCode
        ElapsedSec     = $elapsedSec
        PeakPrivateMB  = $peakPrivateMB
        MinHeadroomMB  = $minObservedHeadroomMB
        PeakGpuLocalMB = $peakGpuLocalMB
        RunDirectory   = $runDir
    }
    $updatedHistory = @($allHistory) + @($entry)
    Save-LaunchHistory -Path $launchHistoryPath -Entries $updatedHistory
}

# 7b. Extract observable rendering & progression evidence from stderr.log and captured frames.
$stderrRaw = if (Test-Path -LiteralPath $stderrPath) { Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue } else { '' }
if ($null -eq $stderrRaw) { $stderrRaw = '' }
$stderrClean = [regex]::Replace($stderrRaw, '\x1b\[[0-9;]*m', '')

$maxFramePresented = 0
foreach ($m in [regex]::Matches($stderrClean, 'frame presented[^\r\n]*?\bframe=(\d+)')) {
    $f = [int]$m.Groups[1].Value
    if ($f -gt $maxFramePresented) { $maxFramePresented = $f }
}
$uniquePipelines = New-Object 'System.Collections.Generic.HashSet[string]'
foreach ($m in [regex]::Matches($stderrClean, 'first bind[^\r\n]*?\bpipeline=(0x[0-9a-fA-F]+)')) {
    [void]$uniquePipelines.Add($m.Groups[1].Value)
}
$uniquePipelinesBound = $uniquePipelines.Count
$scriptedInputsInjected = [regex]::Matches($stderrClean, 'scripted input injected').Count
$maxDrawsPerFrame = 0
foreach ($m in [regex]::Matches($stderrClean, '(\d+)\s+draws/frame')) {
    $d = [int]$m.Groups[1].Value
    if ($d -gt $maxDrawsPerFrame) { $maxDrawsPerFrame = $d }
}

$capturedFrameCount = 0
$uniqueFrameHashes = 0
$framesSubdir = Join-Path $runDir 'frames'
if (Test-Path -LiteralPath $framesSubdir) {
    $pngs = @(Get-ChildItem -Path $framesSubdir -Filter 'frame-*.png' -ErrorAction SilentlyContinue)
    $capturedFrameCount = $pngs.Count
    $hashSet = New-Object 'System.Collections.Generic.HashSet[string]'
    foreach ($png in $pngs) {
        $h = (Get-FileHash -LiteralPath $png.FullName -Algorithm SHA256).Hash
        [void]$hashSet.Add($h)
    }
    $uniqueFrameHashes = $hashSet.Count
}

$progressionStage = 'Boot'
if ($maxFramePresented -gt 0) {
    if ($uniquePipelinesBound -ge 25 -or $maxDrawsPerFrame -ge 80) {
        $progressionStage = 'HeavySceneOrWorldLoading'
    } elseif ($uniquePipelinesBound -ge 7 -or $uniqueFrameHashes -ge 3) {
        $progressionStage = 'UIOrLoadingTransition'
    } elseif ($uniquePipelinesBound -ge 4 -or $uniqueFrameHashes -ge 2) {
        $progressionStage = 'AccountPickerOrStaticUI'
    } else {
        $progressionStage = 'EarlyFramePresentation'
    }
}

# 8. Write JSON summary and Markdown report.
$summary = [ordered]@{
    TimestampUtc              = $runEndUtc.ToString('yyyy-MM-ddTHH:mm:ssZ')
    Mode                      = $modeTag
    LaunchNumberTotal         = $launchNumberTotal
    LaunchNumber24h           = $launchNumber24h
    MaxLaunchesPer24h         = $MaxLaunchesPer24h
    TestLabel                 = $TestLabel
    Branch                    = $branch
    Commit                    = $commit
    ShortCommit               = $shortCommit
    DirtyWorkingTree          = $isDirty
    BuildDurationSec          = $buildDurationSec
    ElapsedSec                = $elapsedSec
    ExitCode                  = $exitCode
    HarnessStopReason         = $harnessStopReason
    Classification            = $classification
    ProgressionStage          = $progressionStage
    MaxFramePresented         = $maxFramePresented
    UniquePipelinesBound      = $uniquePipelinesBound
    MaxDrawsPerFrame          = $maxDrawsPerFrame
    ScriptedInputsInjected    = $scriptedInputsInjected
    CapturedFrameCount        = $capturedFrameCount
    UniqueCapturedFrameHashes = $uniqueFrameHashes
    BaselineCommitMB          = $baselineMem.CommitTotalMB
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
    SyncPipelines         = [bool]$SyncPipelines
    PipelineCacheDisabled = [bool]$DisablePipelineCache
    IsolatedCacheDir      = $IsolatedCacheDir
    RunDirectory          = $runDir
    HardwareRun           = [bool]$hardwareRun
    Preflight             = $preflightReport
    TestsPassed           = $testSummary.Passed
    TestsFailed           = $testSummary.Failed
    TestsIgnored          = $testSummary.Ignored
    TestResultLines       = $testSummary.ResultLines
    FailedTests           = @($testSummary.FailedTests)
    IgnoredTests          = @($testSummary.IgnoredTests)
    SkippedOrUnsupported  = @($testSummary.SkippedLines)
    SkippedByProbe        = @($probeSkips)
    DebugLayerRequested   = [bool]$EnableDebugLayer
    DebugLayerByCrate     = $debugVerdict
    ProbeAdapters         = @($probeAdapters)
    ProbeRecords          = @($probe.Records).Count
    ProbeUnparsable       = $probe.Unparsable
    TreeProcessesSeen     = $knownTree.Count
    TreeTerminated        = @($treeTerminated)
    StragglersTerminated  = @($stragglers)
    TreeRemaining         = @($treeRemaining)
}

$summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $summaryPath -Encoding UTF8

$gpuLocalStr = if ($null -ne $peakGpuLocalMB) { "$peakGpuLocalMB MB" } else { 'N/A (counter unavailable)' }
$gpuNonLocalStr = if ($null -ne $peakGpuNonLocalMB) { "$peakGpuNonLocalMB MB" } else { 'N/A (counter unavailable)' }

$mdLines = @(
    "# HUST-VIctory Diagnostic Report ($modeTag)",
    "",
    "- **Timestamp (UTC)**: $($summary.TimestampUtc)",
    "- **Branch / Commit**: ``$branch`` @ ``$shortCommit`` (full: ``$commit``)$(if ($isDirty) { ' [DIRTY]' } else { '' })",
    "- **Launch Count**: $(if ($hardwareRun) { if ($MaxLaunchesPer24h -gt 0) { "#$launchNumberTotal total (#$launchNumber24h / $MaxLaunchesPer24h in 24h)" } else { "#$launchNumberTotal total (#$launchNumber24h in 24h; no daily quota)" } } else { 'N/A (synthetic)' })",
    "- **Label**: $(if ($TestLabel) { $TestLabel } else { 'default' })",
    "- **Classification**: **$classification** (harness stop: ``$harnessStopReason``, exit code: ``$exitCode``)",
    "- **Duration**: ${elapsedSec}s (max ${MaxDurationSeconds}s; build ${buildDurationSec}s)",
    "- **Switches**: $switchSummary, CommitGuard=ON",
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
    "## Preflight, Tests and Debug Layer",
    "",
    "- **Hardware run**: $hardwareRun; preflight: $preflightReport",
    "- **Tests**: $($testSummary.Passed) passed, $($testSummary.Failed) failed, $($testSummary.Ignored) ignored ($($testSummary.ResultLines) result lines)",
    "- **Skipped or unsupported (counted as passed by cargo)**: $(if ($testSummary.SkippedCount -gt 0) { ($testSummary.SkippedLines | ForEach-Object { '``' + $_ + '``' }) -join '; ' } else { 'none in output' })",
    "- **Skipped tests reported through the probe file**: $(if ($probeSkips.Count -gt 0) { ($probeSkips | ForEach-Object { '``' + $_ + '``' }) -join '; ' } else { 'none' })",
    "- **Debug layer** (requested: $([bool]$EnableDebugLayer)): $(($debugVerdict.PSObject.Properties | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ', ')",
    "- **Adapters seen by probes**: $(if ($probeAdapters.Count -gt 0) { $probeAdapters -join '; ' } else { 'none recorded' })",
    "- **Process tree**: $($knownTree.Count) seen; terminated at stop: $(@($treeTerminated).Count); stragglers after exit: $(@($stragglers).Count); still running: $(@($treeRemaining).Count)",
    "",
    "## Progression & Input Evidence",
    "",
    "| Metric | Value |",
    "|---|---:|",
    "| Observable progression stage | $progressionStage |",
    "| Max frame presented | $maxFramePresented |",
    "| Unique pipelines bound (``first bind``) | $uniquePipelinesBound |",
    "| Peak draws/frame logged | $maxDrawsPerFrame |",
    "| Scripted inputs injected | $scriptedInputsInjected |",
    "| Captured PNG frames (unique SHA-256) | $capturedFrameCount ($uniqueFrameHashes unique) |",
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
Write-Host "Tests:          $($testSummary.Passed) passed, $($testSummary.Failed) failed, $($testSummary.Ignored) ignored; skipped/unsupported: $($testSummary.SkippedCount) in output, $($probeSkips.Count) via probe"
Write-Host "Debug layer:    requested=$([bool]$EnableDebugLayer); $(($debugVerdict.PSObject.Properties | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ', ')"
Write-Host "Process tree:   seen $($knownTree.Count), terminated $(@($treeTerminated).Count), stragglers $(@($stragglers).Count), remaining $(@($treeRemaining).Count)"
Write-Host "Peak Private:   $peakPrivateMB MB | Min Commit Headroom: $minObservedHeadroomMB MB"
Write-Host "Report:         $reportPath"

# 9. Notify Discord if configured and enabled.
if (-not $NoDiscord -and (Test-Path -LiteralPath $discordScript)) {
    $headroomGB = [Math]::Round($minObservedHeadroomMB / 1024.0, 2)
    $labelPart = if ($TestLabel) { " ($TestLabel)" } else { '' }
    $header = if (-not $isSynthetic) { "GPU Test #$launchNumberTotal$labelPart" } elseif ($hardwareRun) { "Synthetic Hardware Diagnostic #$launchNumberTotal$labelPart" } else { "Synthetic CPU Diagnostic" }
    $desc = "$header — Stage: $progressionStage (frame=$maxFramePresented, pipes=$uniquePipelinesBound, inputs=$scriptedInputsInjected, frames=$capturedFrameCount/$uniqueFrameHashes uniq) | Duration: ${elapsedSec}s | Result: $classification (exit=$exitCode) | Peak private: ${peakPrivateMB} MB | GPU local: $gpuLocalStr | Min headroom: ${headroomGB} GB"
    $next = if ($classification -like 'D3D12*') {
        "Analyze D3D12 Debug/DRED/stderr logs in $runDir offline before retrying."
    } elseif ($classification -like '*Commit*') {
        "Inspect memory-*.log commit breakdown in $runDir."
    } else {
        "Inspect report.md, stderr.log, and samples.csv in $runDir."
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

[PSCustomObject]$summary
