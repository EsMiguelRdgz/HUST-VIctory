#Requires -Version 5.1
<#
.SYNOPSIS
    Pure helpers for Run-VictoryDiagnostic.ps1, dot-sourced by the harness
    and by its self-test (Test-VictoryDiagnostic.ps1).
.DESCRIPTION
    Everything here takes its inputs as parameters so the self-test can feed
    synthetic data: preflight decisions, process-tree collection and
    termination, cargo test output parsing (skips reported, never hidden),
    and the test probe file the D3D12 tests write.
#>

Set-StrictMode -Version Latest

# Process names of this repository's test binaries (cargo names them
# <crate>-<hash>.exe) and of the title and our launcher.
$script:VictoryProcessPatterns = @('victory', 'game_scarlett_master', 'victory_gpu-*', 'victory_shader-*', 'victory_hle-*', 'victory_core-*')

function Get-VictoryPreflightProblems {
    <#
    .SYNOPSIS
        Returns the reasons a hardware run (title or synthetic) must not start;
        empty when every check passes. Also returns how long to wait first.
    .PARAMETER Adapters
        Display adapters (objects with Name, Status, ConfigManagerErrorCode),
        as Win32_VideoController returns them.
    .PARAMETER RecentTdrEvents
        Display Event 4101 records from the last 3 minutes.
    .PARAMETER RunningProcessNames
        Names of running processes.
    #>
    [CmdletBinding()]
    param(
        [object[]]$Adapters = @(),
        [object[]]$RecentTdrEvents = @(),
        [string[]]$RunningProcessNames = @(),
        [double]$CommitHeadroomMB,
        [double]$MinPreLaunchHeadroomGB,
        [Nullable[datetime]]$LastLaunchEndUtc = $null,
        [datetime]$NowUtc = [datetime]::UtcNow,
        [int]$RecoveryIntervalSeconds = 60
    )
    $problems = New-Object System.Collections.Generic.List[string]
    $busy = @($RunningProcessNames | Where-Object {
            $n = $_
            @($script:VictoryProcessPatterns | Where-Object { $n -like $_ }).Count -gt 0
        } | Sort-Object -Unique)
    if ($busy.Count -gt 0) {
        $problems.Add("existing VIctory/game/test process already running: $($busy -join ', ')")
    }
    $nv = @($Adapters | Where-Object { $_.Name -like '*NVIDIA*' }) | Select-Object -First 1
    if ($null -eq $nv) {
        $problems.Add('no NVIDIA display adapter found')
    } elseif ($nv.Status -ne 'OK' -or ($null -ne $nv.ConfigManagerErrorCode -and $nv.ConfigManagerErrorCode -ne 0)) {
        $problems.Add("NVIDIA adapter '$($nv.Name)' unhealthy (Status='$($nv.Status)', ConfigManagerErrorCode=$($nv.ConfigManagerErrorCode))")
    }
    if (@($RecentTdrEvents).Count -gt 0) {
        $problems.Add("Windows recorded a display driver reset (Event 4101) less than 3 minutes ago")
    }
    $floorMB = $MinPreLaunchHeadroomGB * 1024.0
    if ($CommitHeadroomMB -lt $floorMB) {
        $problems.Add(("system commit headroom {0:N0} MB is below the {1:N0} MB pre-launch floor" -f $CommitHeadroomMB, $floorMB))
    }
    $wait = 0
    if ($null -ne $LastLaunchEndUtc -and $RecoveryIntervalSeconds -gt 0) {
        $since = ($NowUtc - [datetime]$LastLaunchEndUtc).TotalSeconds
        if ($since -lt $RecoveryIntervalSeconds) { $wait = [int][Math]::Ceiling($RecoveryIntervalSeconds - $since) }
    }
    return [PSCustomObject]@{ Problems = $problems.ToArray(); WaitSeconds = $wait }
}

function Get-VictoryDescendants {
    <#
    .SYNOPSIS
        Processes descended from RootId in a snapshot of Win32_Process
        objects (ProcessId, ParentProcessId, CreationDate).
    .DESCRIPTION
        PIDs are reused, so parenthood is checked against creation times:
        a child must not predate its parent. The root may already have exited
        (its PID possibly reused): its children count only when created at or
        after RootCreationDate and, when RootChildrenNotAfter is given, not
        after it (the root's exit). Other parents are taken from the snapshot.
    #>
    [CmdletBinding()]
    param(
        [int]$RootId,
        [object[]]$Snapshot,
        [Nullable[datetime]]$RootCreationDate = $null,
        [Nullable[datetime]]$RootChildrenNotAfter = $null
    )
    $byParent = @{}
    $created = @{}
    foreach ($p in $Snapshot) {
        $created[[int]$p.ProcessId] = $p.CreationDate
        $key = [int]$p.ParentProcessId
        if (-not $byParent.ContainsKey($key)) { $byParent[$key] = New-Object System.Collections.Generic.List[object] }
        $byParent[$key].Add($p)
    }
    $out = New-Object System.Collections.Generic.List[object]
    $queue = New-Object System.Collections.Generic.Queue[int]
    $seen = @{ $RootId = $true }
    $queue.Enqueue($RootId)
    while ($queue.Count -gt 0) {
        $parent = $queue.Dequeue()
        if (-not $byParent.ContainsKey($parent)) { continue }
        foreach ($c in $byParent[$parent]) {
            $id = [int]$c.ProcessId
            if ($seen.ContainsKey($id)) { continue }
            if ($parent -eq $RootId) {
                $since = if ($null -ne $RootCreationDate) { [datetime]$RootCreationDate } elseif ($created.ContainsKey($parent)) { $created[$parent] } else { $null }
                if ($null -ne $since -and $null -ne $c.CreationDate -and $c.CreationDate -lt $since) { continue }
                if ($null -ne $RootChildrenNotAfter -and $null -ne $c.CreationDate -and $c.CreationDate -gt [datetime]$RootChildrenNotAfter) { continue }
            } elseif ($created.ContainsKey($parent) -and $null -ne $created[$parent] -and $null -ne $c.CreationDate -and $c.CreationDate -lt $created[$parent]) {
                continue
            }
            $seen[$id] = $true
            $out.Add($c)
            $queue.Enqueue($id)
        }
    }
    return $out.ToArray()
}

function Update-VictoryKnownTree {
    <#
    .SYNOPSIS
        Adds the root's current descendants, and those of every known
        process still alive (same creation time), to Known (a hashtable of
        pid -> CreationDate), so processes whose parent already exited are
        still terminated later.
    #>
    [CmdletBinding()]
    param(
        [int]$RootId,
        [hashtable]$Known,
        [Nullable[datetime]]$RootCreationDate = $null,
        [Nullable[datetime]]$RootChildrenNotAfter = $null,
        [object[]]$Snapshot = $null
    )
    if ($null -eq $Snapshot) {
        try {
            $Snapshot = @(Get-CimInstance -ClassName Win32_Process -Property ProcessId, ParentProcessId, CreationDate -ErrorAction Stop)
        } catch {
            return
        }
    }
    foreach ($d in @(Get-VictoryDescendants -RootId $RootId -Snapshot $Snapshot -RootCreationDate $RootCreationDate -RootChildrenNotAfter $RootChildrenNotAfter)) {
        $Known[[int]$d.ProcessId] = $d.CreationDate
    }
    $alive = @{}
    foreach ($p in $Snapshot) { $alive[[int]$p.ProcessId] = $p.CreationDate }
    foreach ($k in @($Known.Keys)) {
        if (-not $alive.ContainsKey([int]$k) -or $alive[[int]$k] -ne $Known[$k]) { continue }
        foreach ($d in @(Get-VictoryDescendants -RootId ([int]$k) -Snapshot $Snapshot -RootCreationDate $Known[$k])) {
            $Known[[int]$d.ProcessId] = $d.CreationDate
        }
    }
}

function Stop-VictoryProcessTree {
    <#
    .SYNOPSIS
        Terminates RootId and every descendant, current ones and those
        recorded earlier in Known (only while their creation time still
        matches, so a reused PID is never killed). Deepest first. Returns
        the terminated and still-running PIDs.
    #>
    [CmdletBinding()]
    param(
        [int]$RootId,
        [hashtable]$Known = @{},
        # The root's creation time, taken when it started: the root is killed
        # only while its PID still names that process.
        [Nullable[datetime]]$RootCreationDate = $null,
        # When the root already exited: its children must predate this.
        [Nullable[datetime]]$RootExitedAt = $null,
        [int]$WaitMilliseconds = 10000
    )
    $targets = @{}
    $root = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $RootId" -ErrorAction SilentlyContinue
    if ($null -ne $root -and ($null -eq $RootCreationDate -or $root.CreationDate -eq [datetime]$RootCreationDate)) {
        $targets[$RootId] = $root.CreationDate
    }
    $killed = New-Object System.Collections.Generic.List[int]
    # Up to three rounds: a process may start a child while the tree is
    # being terminated; each round snapshots the tree again first.
    for ($round = 0; $round -lt 3; $round++) {
        Update-VictoryKnownTree -RootId $RootId -Known $Known -RootCreationDate $RootCreationDate -RootChildrenNotAfter $RootExitedAt
        foreach ($k in $Known.Keys) { $targets[[int]$k] = $Known[$k] }
        $alive = 0
        # Every descendant first, the root last (it cannot respawn them then).
        $order = @($targets.Keys | Where-Object { $_ -ne $RootId }) + @($RootId)
        foreach ($id in $order) {
            if (-not $targets.ContainsKey($id)) { continue }
            $p = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $id" -ErrorAction SilentlyContinue
            if ($null -eq $p) { continue }
            if ($null -ne $targets[$id] -and $p.CreationDate -ne $targets[$id]) { continue } # PID reused
            $alive++
            try {
                Stop-Process -Id $id -Force -ErrorAction Stop
                if (-not $killed.Contains($id)) { $killed.Add($id) }
            } catch {
                # Exited meanwhile.
            }
        }
        if ($alive -eq 0) { break }
        Start-Sleep -Milliseconds 200
    }
    $deadline = [datetime]::UtcNow.AddMilliseconds($WaitMilliseconds)
    $remaining = @()
    do {
        $remaining = @($targets.Keys | Where-Object {
                $p = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $_" -ErrorAction SilentlyContinue
                $null -ne $p -and ($null -eq $targets[$_] -or $p.CreationDate -eq $targets[$_])
            })
        if ($remaining.Count -eq 0) { break }
        Start-Sleep -Milliseconds 200
    } while ([datetime]::UtcNow -lt $deadline)
    return [PSCustomObject]@{ Terminated = $killed.ToArray(); Remaining = @($remaining) }
}

function Get-CargoTestSummary {
    <#
    .SYNOPSIS
        Totals of every `test result:` line, ignored test names, failures,
        and every line reporting a skipped or unavailable dependency (a test
        that returns early prints one; it still counts as "passed").
    #>
    [CmdletBinding()]
    param([string]$Text)
    $clean = [regex]::Replace($Text, '\x1b\[[0-9;]*m', '')
    $passed = 0; $failed = 0; $ignored = 0; $results = 0
    foreach ($m in [regex]::Matches($clean, '(?m)^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored')) {
        $results++
        $passed += [int]$m.Groups[1].Value
        $failed += [int]$m.Groups[2].Value
        $ignored += [int]$m.Groups[3].Value
    }
    $ignoredNames = @([regex]::Matches($clean, '(?m)^test (\S+) \.\.\. ignored') | ForEach-Object { $_.Groups[1].Value })
    $failedNames = @([regex]::Matches($clean, '(?m)^test (\S+) \.\.\. FAILED') | ForEach-Object { $_.Groups[1].Value })
    $skips = @([regex]::Matches($clean, '(?im)^(?!test result:)(?!test \S+ \.\.\. ignored).*\b(skipped|unsupported|not available|unavailable)\b.*$') |
            ForEach-Object { $_.Value.Trim() } | Where-Object { $_ -notmatch '^(running|Running) ' })
    return [PSCustomObject]@{
        ResultLines   = $results
        Passed        = $passed
        Failed        = $failed
        Ignored       = $ignored
        IgnoredTests  = $ignoredNames
        FailedTests   = $failedNames
        SkippedLines  = $skips
        SkippedCount  = $skips.Count
    }
}

function Read-VictoryTestProbe {
    <#
    .SYNOPSIS
        Reads the JSON lines the D3D12 probe tests append to
        VICTORY_TEST_PROBE_FILE. Unparsable lines are reported, not dropped.
    #>
    [CmdletBinding()]
    param([string]$Path)
    $records = New-Object System.Collections.Generic.List[object]
    $bad = 0
    if (-not [string]::IsNullOrWhiteSpace($Path) -and (Test-Path -LiteralPath $Path)) {
        foreach ($line in [System.IO.File]::ReadAllLines($Path)) {
            if ([string]::IsNullOrWhiteSpace($line)) { continue }
            try { $records.Add(($line | ConvertFrom-Json)) } catch { $bad++ }
        }
    }
    return [PSCustomObject]@{ Records = $records.ToArray(); Unparsable = $bad }
}

# Packages whose tests or binaries reach victory-gpu's host device. Its
# software-adapter switch (VICTORY_D3D12_WARP) works only with the
# victory-gpu `diagnostics` feature, which each of these packages'
# `diagnostics` feature enables.
$script:GpuReachingPackages = @('victory-gpu', 'victory-hle', 'victory')

function Get-VictoryCargoSelection {
    <#
    .SYNOPSIS
        What a cargo command line selects: subcommand, packages (empty =
        the whole workspace), features, --all-features. Only arguments
        before `--` (the test binary's own arguments) are read.
    #>
    [CmdletBinding()]
    param([string[]]$Arguments = @())
    $own = New-Object System.Collections.Generic.List[string]
    foreach ($a in $Arguments) { if ($a -eq '--') { break }; $own.Add($a) }
    $sub = $null; $packages = New-Object System.Collections.Generic.List[string]
    $features = New-Object System.Collections.Generic.List[string]
    $workspace = $false; $allFeatures = $false
    for ($i = 0; $i -lt $own.Count; $i++) {
        $a = $own[$i]
        $value = $null
        if ($a -in @('-p', '--package', '-F', '--features') -and $i + 1 -lt $own.Count) { $value = $own[$i + 1] }
        if ($a -in @('-p', '--package')) { if ($null -ne $value) { $packages.Add($value); $i++ }; continue }
        if ($a -match '^(--package=|-p)(.+)$') { $packages.Add($Matches[2]); continue }
        if ($a -in @('-F', '--features')) { if ($null -ne $value) { $value -split '[,\s]+' | Where-Object { $_ } | ForEach-Object { $features.Add($_) }; $i++ }; continue }
        if ($a -match '^--features=(.+)$') { $Matches[1] -split '[,\s]+' | Where-Object { $_ } | ForEach-Object { $features.Add($_) }; continue }
        if ($a -in @('--workspace', '--all')) { $workspace = $true; continue }
        if ($a -eq '--all-features') { $allFeatures = $true; continue }
        if ($null -eq $sub -and -not $a.StartsWith('-') -and -not $a.StartsWith('+')) { $sub = $a }
    }
    return [PSCustomObject]@{
        Subcommand  = $sub
        Packages    = $packages.ToArray()
        Workspace   = ($workspace -or $packages.Count -eq 0)
        Features    = $features.ToArray()
        AllFeatures = $allFeatures
    }
}

function Get-VictoryCpuOnlyCargoProblem {
    <#
    .SYNOPSIS
        Why a CPU-only synthetic cargo command could still reach a physical
        GPU, or $null. A command that runs code (test, bench, run, nextest)
        and selects a package reaching victory-gpu must enable
        victory-gpu's `diagnostics` feature, or VICTORY_D3D12_WARP is
        ignored and the host device is the hardware adapter.
    #>
    [CmdletBinding()]
    param([string]$Command, [string[]]$Arguments = @())
    if ([IO.Path]::GetFileNameWithoutExtension($Command) -ne 'cargo') { return $null }
    $sel = Get-VictoryCargoSelection -Arguments $Arguments
    if ($sel.Subcommand -notin @('test', 'bench', 'run', 'nextest', 't', 'r')) { return $null }
    $reaching = @($sel.Packages | Where-Object { $_ -in $script:GpuReachingPackages })
    if (-not $sel.Workspace -and $reaching.Count -eq 0) { return $null }
    if ($sel.AllFeatures) { return $null }
    $qualified = @($sel.Features | Where-Object { $_ -in @('victory-gpu/diagnostics', 'victory-hle/diagnostics', 'victory/diagnostics') })
    if ($qualified.Count -gt 0) { return $null }
    if (-not $sel.Workspace -and $sel.Packages.Count -eq 1 -and $reaching.Count -eq 1 -and $sel.Features -contains 'diagnostics') { return $null }
    $what = if ($sel.Workspace) { 'the whole workspace' } else { $reaching -join ', ' }
    return "CPU-only cargo $($sel.Subcommand) of $what without victory-gpu's diagnostics feature: VICTORY_D3D12_WARP would be ignored and victory-gpu would use the hardware adapter. Add --features victory-gpu/diagnostics."
}

function Get-VictoryAdapterEvidenceProblems {
    <#
    .SYNOPSIS
        CPU-only adapter evidence from probe records: any adapter that is
        not software is a violation; a probed crate the command ran that
        left no adapter record is missing evidence.
    #>
    [CmdletBinding()]
    param([object[]]$Records = @(), [string[]]$ExpectedCrates = @())
    $violations = New-Object System.Collections.Generic.List[string]
    $missing = New-Object System.Collections.Generic.List[string]
    foreach ($r in $Records) {
        if ($r.PSObject.Properties['adapter'] -and $r.PSObject.Properties['software'] -and -not $r.software) {
            $crate = if ($r.PSObject.Properties['crate']) { $r.crate } else { '?' }
            $violations.Add("${crate}: $($r.adapter) is not a software adapter")
        }
    }
    foreach ($c in $ExpectedCrates) {
        $seen = @($Records | Where-Object { $_.PSObject.Properties['crate'] -and $_.crate -eq $c -and $_.PSObject.Properties['adapter'] })
        if ($seen.Count -eq 0) { $missing.Add("${c}: no adapter evidence") }
    }
    return [PSCustomObject]@{ Violations = $violations.ToArray(); Missing = $missing.ToArray() }
}

function Get-VictoryProbedCrates {
    <#
    .SYNOPSIS
        The crates with an adapter probe test that a cargo test command runs.
    #>
    [CmdletBinding()]
    param([string]$Command, [string[]]$Arguments = @())
    if ([IO.Path]::GetFileNameWithoutExtension($Command) -ne 'cargo') { return @() }
    $sel = Get-VictoryCargoSelection -Arguments $Arguments
    if ($sel.Subcommand -notin @('test', 't')) { return @() }
    $probed = @('victory-gpu', 'victory-shader')
    if ($sel.Workspace) { return $probed }
    return @($sel.Packages | Where-Object { $_ -in $probed })
}

function Get-VictoryProbeSkips {
    <#
    .SYNOPSIS
        Skips the tests reported through the probe file (cargo hides the
        stderr of passing tests, so this is how an early return is seen):
        `skip` records, and `adapter` records of an unavailable device.
    #>
    [CmdletBinding()]
    param([object[]]$Records = @())
    $out = New-Object System.Collections.Generic.List[string]
    foreach ($r in $Records) {
        $kind = if ($r.PSObject.Properties['probe']) { $r.probe } else { '' }
        $crate = if ($r.PSObject.Properties['crate']) { $r.crate } else { '?' }
        if ($kind -eq 'skip') {
            $test = if ($r.PSObject.Properties['test']) { $r.test } else { '?' }
            $reason = if ($r.PSObject.Properties['reason']) { $r.reason } else { '?' }
            $out.Add("${crate} ${test}: $reason")
        } elseif ($kind -eq 'adapter' -and $r.PSObject.Properties['unsupported']) {
            $label = if ($r.PSObject.Properties['label']) { $r.label } else { '?' }
            $out.Add("${crate} ${label} device unavailable: $($r.unsupported)")
        }
    }
    return $out.ToArray()
}

function Get-VictoryDebugLayerVerdict {
    <#
    .SYNOPSIS
        Per-crate debug-layer report from probe records: Active, NotActive
        (requested but not loaded), NotEnabledByTestHost (the crate's test
        host never enables it), NotRequested, or NoProbe (no evidence).
    #>
    [CmdletBinding()]
    param([object[]]$Records = @(), [bool]$Requested, [string[]]$Crates = @('victory-gpu', 'victory-shader'))
    $out = [ordered]@{}
    foreach ($crate in $Crates) {
        $r = @($Records | Where-Object { $_.PSObject.Properties['crate'] -and $_.crate -eq $crate -and $_.PSObject.Properties['probe'] -and $_.probe -eq 'd3d12-debug-layer' }) | Select-Object -Last 1
        if ($null -eq $r) {
            $out[$crate] = 'NoProbe'
        } elseif ($r.PSObject.Properties['supported'] -and -not $r.supported) {
            $out[$crate] = 'NotEnabledByTestHost'
        } elseif (-not $Requested) {
            $out[$crate] = 'NotRequested'
        } elseif ($r.active) {
            $out[$crate] = 'Active'
        } else {
            $out[$crate] = 'NotActive'
        }
    }
    return [PSCustomObject]$out
}
