#Requires -Version 5.1
<#
.SYNOPSIS
    Static safety check for the HUST-VIctory hardware diagnostic path.
.DESCRIPTION
    Reads text only; never launches anything. Fails (exit 1) when:

      * tools/diag/Run-VictoryDiagnostic.ps1 lost one of its RAM / commit /
        GPU-health / single-instance / timeout / authorization guards;
      * a workflow or tool script disables the VIctory commit guard, writes a
        Windows TDR setting, or weakens operating-system protections;
      * the home RTX 3070 workflow gained an automatic trigger, stopped
        targeting only the `hust-rtx3070` runner, downloads files, or uploads
        anything other than the sanitized diagnostic bundle;
      * a workflow or tool script contains something that looks like a
        literal credential.

    The CI workflow runs it on every commit it checks. The hardware workflow
    runs the copy from the workflow's own (default-branch) revision against
    the commit selected for testing, so a commit cannot weaken the guards and
    test itself.
.PARAMETER RepoRoot
    The checkout to inspect. Defaults to the repository containing this script.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# $PSScriptRoot is empty inside param() defaults on Windows PowerShell 5.1.
if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = Join-Path $PSScriptRoot '..\..' }

$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path.TrimEnd('\', '/')
$failures = New-Object System.Collections.Generic.List[string]
$checked = 0

function Add-Failure([string]$Message) {
    $script:failures.Add($Message)
}

function Read-RepoText([string]$Relative) {
    $path = Join-Path $RepoRoot $Relative
    if (-not (Test-Path -LiteralPath $path)) {
        Add-Failure "$Relative is missing."
        return $null
    }
    return [System.IO.File]::ReadAllText($path)
}

# Removes PowerShell / YAML comments so a guard that only survives inside a
# comment does not count as present.
function Remove-Comments([string]$Text, [string]$Kind) {
    if ($Kind -eq 'ps1') {
        $Text = [regex]::Replace($Text, '(?s)<#.*?#>', '')
    }
    $lines = foreach ($line in ($Text -split "`r?`n")) {
        if ($line -match '^\s*#') { '' } else { $line }
    }
    return ($lines -join "`n")
}

# ---------------------------------------------------------------------------
# 1. The diagnostic harness keeps every guard.
# ---------------------------------------------------------------------------
$harnessRel = 'tools/diag/Run-VictoryDiagnostic.ps1'
$harness = Read-RepoText $harnessRel
if ($null -ne $harness) {
    $checked++
    $code = Remove-Comments $harness 'ps1'
    $required = [ordered]@{
        'explicit -AuthorizeGameLaunch gate'                  = 'if\s*\(\s*-not\s+\$AuthorizeGameLaunch\s*\)\s*\{\s*throw'
        'runtime commit floor >= 5 GB (hard limit)'           = '\$MinCommitHeadroomGB\s+-lt\s+5\.0\b[^\r\n]*\)\s*\{\s*throw'
        'pre-launch commit floor >= 10 GB (hard limit)'       = '\$MinPreLaunchHeadroomGB\s+-lt\s+10\.0\b[^\r\n]*\)\s*\{\s*throw'
        'runtime commit headroom cutoff'                      = "\`$harnessStopReason\s*=\s*'HarnessCommitHeadroomGuard'"
        'pre-launch headroom refusal'                         = 'Refusing launch: preexisting system commit headroom'
        'max duration cutoff'                                 = "\`$harnessStopReason\s*=\s*'MaxDurationReached'"
        'VIctory commit guard kept on'                        = "EnvironmentVariables\.Remove\('VICTORY_MEMORY_GUARD'\)"
        'single-instance check'                               = "Get-Process\s+-Name\s+'victory',\s*'game_scarlett_master'"
        'NVIDIA adapter health check'                         = "Win32_VideoController"
        'recent display-driver reset (Event 4101) check'      = 'Id\s*=\s*4101'
        'GPU recovery interval'                               = '\$RecoveryIntervalSeconds\s+-gt\s+0'
        'launch history outside the repository'               = 'hw-launch-history\.json'
        'synthetic runs are hardware runs unless CPU-only'    = '\$hardwareRun\s*=\s*-not\s+\$isSynthetic\s+-or\s+-not\s+\$SyntheticCpuOnly'
        'hardware preflight gate covers synthetic runs'       = 'if\s*\(\s*\$hardwareRun\s*\)\s*\{[\s\S]*?Get-VictoryPreflightProblems[\s\S]*?\{\s*throw'
        'CPU-only runs forced onto the software adapter'      = "EnvironmentVariables\['VICTORY_D3D12_WARP'\]\s*=\s*'1'"
        'CPU-only cargo runs that could reach a GPU refused'  = 'Get-VictoryCpuOnlyCargoProblem[^\r\n]*\r?\n\s*if\s*\(\s*\$null\s+-ne\s+\$cpuOnlyProblem\s*\)\s*\{\s*throw'
        'CPU-only adapter evidence required'                  = "'CpuOnlyAdapterViolation'[\s\S]*?'AdapterNotVerified'"
        'timeout terminates the whole process tree'           = "'MaxDurationReached'\s*\r?\n[^\r\n]*\r?\n\s*\`$stop\s*=\s*Stop-VictoryProcessTree"
        'guard stop terminates the whole process tree'        = "floor\.`"\s*-f[^\r\n]*\r?\n[^\r\n]*\r?\n\s*\`$stop\s*=\s*Stop-VictoryProcessTree"
    }
    foreach ($name in $required.Keys) {
        if ($code -notmatch $required[$name]) {
            Add-Failure "${harnessRel}: guard missing or altered: $name."
        }
    }
    # Default values must not be loosened.
    $defaults = [ordered]@{
        'MinPreLaunchHeadroomGB default 10.0' = '\[double\]\$MinPreLaunchHeadroomGB\s*=\s*(\d+(?:\.\d+)?)'
        'MinCommitHeadroomGB default 5.0'     = '\[double\]\$MinCommitHeadroomGB\s*=\s*(\d+(?:\.\d+)?)'
        'RecoveryIntervalSeconds default 60'  = '\[int\]\$RecoveryIntervalSeconds\s*=\s*(\d+)'
    }
    $minimums = @{ 'MinPreLaunchHeadroomGB default 10.0' = 10.0; 'MinCommitHeadroomGB default 5.0' = 5.0; 'RecoveryIntervalSeconds default 60' = 60 }
    foreach ($name in $defaults.Keys) {
        $m = [regex]::Match($code, $defaults[$name])
        if (-not $m.Success) {
            Add-Failure "${harnessRel}: parameter default not found: $name."
        } elseif ([double]$m.Groups[1].Value -lt $minimums[$name]) {
            Add-Failure "${harnessRel}: parameter default loosened: $name (found $($m.Groups[1].Value))."
        }
    }
}

# ---------------------------------------------------------------------------
# 2. No workflow or tool script weakens OS / driver / VIctory protections.
# ---------------------------------------------------------------------------
$scan = New-Object System.Collections.Generic.List[System.IO.FileInfo]
foreach ($dir in @('.github/workflows', 'tools')) {
    $full = Join-Path $RepoRoot $dir
    if (Test-Path -LiteralPath $full) {
        # Filter by extension explicitly: Windows PowerShell 5.1 ignores
        # -Include together with -LiteralPath and would scan every file.
        Get-ChildItem -LiteralPath $full -Recurse -File |
            Where-Object { @('.yml', '.yaml', '.ps1', '.cmd', '.sh') -contains $_.Extension.ToLowerInvariant() } |
            ForEach-Object { $scan.Add($_) }
    }
}
$self = (Resolve-Path -LiteralPath $PSCommandPath).Path

$forbidden = [ordered]@{
    'writes a Windows TDR registry value'          = '(?im)^(?!\s*#).*(Set-ItemProperty|New-ItemProperty|reg(\.exe)?\s+add|Remove-ItemProperty|reg(\.exe)?\s+delete)[^\r\n]*Tdr'
    'disables the VIctory commit guard'            = "(?i)VICTORY_MEMORY_GUARD['""]?\s*(\]\s*)?[=:]\s*['""]?0"
    'changes Microsoft Defender settings'          = '(?i)\b(Set-MpPreference|Add-MpPreference|DisableRealtimeMonitoring)\b'
    'changes boot configuration'                   = '(?i)\bbcdedit\b'
    'disables system protection or the firewall'   = '(?i)\b(Disable-ComputerRestore|Set-NetFirewallProfile|netsh\s+advfirewall\s+set)\b'
    'changes the machine execution policy'         = '(?i)Set-ExecutionPolicy[^\r\n]*-Scope\s+LocalMachine'
    'changes the pagefile'                         = '(?i)(Win32_PageFileSetting|AutomaticManagedPagefile)'
    'GitHub token literal'                         = '\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}\b|\bgithub_pat_[A-Za-z0-9_]{40,}\b'
    'Discord webhook URL literal'                  = '(?i)discord(app)?\.com/api/webhooks/\d{5,}/[A-Za-z0-9_\-]{20,}'
    'runner registration token literal'            = '(?i)config\.cmd[^\r\n]*--token\s+[A-Z0-9]{20,}'
    'private key block'                            = '-----BEGIN [A-Z ]*PRIVATE KEY-----'
    'cloud access key literal'                     = '\bAKIA[0-9A-Z]{16}\b'
}
foreach ($file in $scan) {
    if ($file.FullName -eq $self) { continue }
    $checked++
    $text = [System.IO.File]::ReadAllText($file.FullName)
    $rel = $file.FullName.Substring($RepoRoot.Length).TrimStart('\', '/') -replace '\\', '/'
    foreach ($name in $forbidden.Keys) {
        if ($text -match $forbidden[$name]) {
            Add-Failure "${rel}: $name."
        }
    }
}

# ---------------------------------------------------------------------------
# 3. The home RTX 3070 workflow stays manual, targeted and download-free.
# ---------------------------------------------------------------------------
$hwRel = '.github/workflows/home-rtx3070-diagnostic.yml'
$hwPath = Join-Path $RepoRoot $hwRel
if (Test-Path -LiteralPath $hwPath) {
    $checked++
    $hw = Remove-Comments ([System.IO.File]::ReadAllText($hwPath)) 'yml'
    $onBlock = [regex]::Match($hw, '(?ms)^on:\s*\n(.*?)(?=^\S)')
    if (-not $onBlock.Success) {
        Add-Failure "${hwRel}: no top-level 'on:' block."
    } else {
        $triggers = @([regex]::Matches($onBlock.Groups[1].Value, '(?m)^  ([A-Za-z_]+):') | ForEach-Object { $_.Groups[1].Value })
        if ($triggers.Count -ne 1 -or $triggers[0] -ne 'workflow_dispatch') {
            Add-Failure "${hwRel}: must be triggered by workflow_dispatch only (found: $($triggers -join ', '))."
        }
    }
    $runsOn = @([regex]::Matches($hw, '(?m)^\s*runs-on:\s*(.+)$') | ForEach-Object { $_.Groups[1].Value.Trim() })
    $selfHosted = @($runsOn | Where-Object { $_ -match 'self-hosted' })
    if ($selfHosted.Count -ne 1) {
        Add-Failure "${hwRel}: expected exactly one self-hosted job (found $($selfHosted.Count))."
    }
    foreach ($r in $selfHosted) {
        if ($r -notmatch '\bhust-rtx3070\b') {
            Add-Failure "${hwRel}: self-hosted job does not require the 'hust-rtx3070' label: $r"
        }
    }
    foreach ($r in ($runsOn | Where-Object { $_ -notmatch 'self-hosted' })) {
        if ($r -notmatch '^(ubuntu|windows)-latest$') {
            Add-Failure "${hwRel}: unexpected runner: $r"
        }
    }
    $hwForbidden = [ordered]@{
        'downloads artifacts'                      = '(?i)actions/download-artifact'
        'downloads files'                          = '(?i)\b(Invoke-WebRequest|Invoke-RestMethod|Start-BitsTransfer|wget|curl|DownloadFile|DownloadString)\b'
        'passes VICTORY_MEMORY_GUARD'              = '(?i)VICTORY_MEMORY_GUARD'
        'runs on pull requests'                    = '(?im)^\s*(pull_request|pull_request_target)\s*:'
        'keeps the GitHub token in the checkout'   = '(?i)persist-credentials:\s*true'
    }
    foreach ($name in $hwForbidden.Keys) {
        if ($hw -match $hwForbidden[$name]) {
            Add-Failure "${hwRel}: $name."
        }
    }
    if ($hw -notmatch 'concurrency:\s*\n\s*group:\s*hust-rtx3070-hardware\s*\n\s*cancel-in-progress:\s*false') {
        Add-Failure "${hwRel}: must serialize hardware runs (concurrency group 'hust-rtx3070-hardware', cancel-in-progress: false)."
    }
    if ($hw -notmatch 'vars\.HUST_RTX3070_HARDWARE_ENABLED' -or $hw -notmatch '\[\s*"\$ENABLED"\s*=\s*"true"\s*\]\s*\|\|\s*fail') {
        Add-Failure "${hwRel}: must refuse unless the owner sets HUST_RTX3070_HARDWARE_ENABLED to 'true'."
    }
    if ($hw -notmatch 'Run-VictoryDiagnostic\.ps1') {
        Add-Failure "${hwRel}: must launch through tools/diag/Run-VictoryDiagnostic.ps1."
    }
    $uploads = @([regex]::Matches($hw, '(?ms)uses:\s*actions/upload-artifact@\S+.*?path:\s*([^\r\n]+)') | ForEach-Object { $_.Groups[1].Value.Trim() })
    foreach ($u in $uploads) {
        if ($u -notmatch 'sanitized') {
            Add-Failure "${hwRel}: uploads something other than the sanitized bundle: $u"
        }
    }
} else {
    Write-Host "note: $hwRel not present in this checkout; workflow checks skipped."
}

# ---------------------------------------------------------------------------
if ($failures.Count -gt 0) {
    foreach ($f in $failures) { Write-Host "SAFETY: $f" }
    Write-Host "hardware safety check: $($failures.Count) problem(s) in $checked file(s)."
    exit 1
}
Write-Host "hardware safety check: $checked file(s) ok."
exit 0
