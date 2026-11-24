#Requires -Version 5.1
<#
.SYNOPSIS
    Copies the shareable part of one diagnostic run into a sanitized bundle.
.DESCRIPTION
    Used by the home RTX 3070 workflow after Run-VictoryDiagnostic.ps1. The
    run directory stays on the home machine untouched; this script writes a
    separate, reduced copy that is safe to upload as a workflow artifact:

      * allowlisted files only: summary.json, report.md, samples.csv,
        memory-*.log, *-crashes.log, and filtered stdout/stderr logs;
      * never frame captures, executables, caches, shader dumps, or anything
        else from the run directory or the title's folders;
      * log lines the title itself printed (tracing target `title`) are
        dropped, because they are the title's own text;
      * local paths, the user name and the machine name are replaced by
        placeholders; ANSI colour codes are stripped;
      * each log keeps at most its last -MaxLogLines lines and every file is
        capped at -MaxFileBytes; the whole bundle at -MaxBundleBytes.

    Fails (exit 1) if the result contains anything that is not text of an
    allowed type.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$RunDirectory,
    [Parameter(Mandatory)] [string]$Destination,
    # Extra absolute paths to replace with placeholders (title folder, results
    # root, runner workspace...). Pass as "PLACEHOLDER=path" pairs.
    [string[]]$Redact = @(),
    [int]$MaxLogLines = 20000,
    [long]$MaxFileBytes = 8MB,
    [long]$MaxBundleBytes = 40MB
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $RunDirectory -PathType Container)) {
    throw "Run directory not found: $RunDirectory"
}
New-Item -ItemType Directory -Path $Destination -Force | Out-Null
if (@(Get-ChildItem -LiteralPath $Destination -Force).Count -gt 0) {
    throw "Destination must be empty: $Destination"
}

# Longest paths first so a nested path is replaced before its parent.
$pairs = New-Object System.Collections.Generic.List[object]
foreach ($r in $Redact) {
    $i = $r.IndexOf('=')
    if ($i -lt 1) { throw "Redact entries must be PLACEHOLDER=path: $r" }
    $name = $r.Substring(0, $i)
    $path = $r.Substring($i + 1).TrimEnd('\', '/')
    if ($path.Length -ge 3) { $pairs.Add([PSCustomObject]@{ Name = "<$name>"; Path = $path }) }
}
if ($env:USERPROFILE) { $pairs.Add([PSCustomObject]@{ Name = '<USERPROFILE>'; Path = $env:USERPROFILE.TrimEnd('\') }) }
$sorted = @($pairs | Sort-Object { $_.Path.Length } -Descending)

function Protect-Text([string]$Text) {
    $Text = [regex]::Replace($Text, '\x1b\[[0-9;]*[A-Za-z]', '')
    foreach ($p in $sorted) {
        foreach ($form in @($p.Path, ($p.Path -replace '\\', '/'), ($p.Path -replace '\\', '\\'))) {
            $Text = [regex]::Replace($Text, [regex]::Escape($form), $p.Name, 'IgnoreCase')
        }
    }
    if ($env:USERNAME -and $env:USERNAME.Length -ge 3) {
        $Text = [regex]::Replace($Text, '\b' + [regex]::Escape($env:USERNAME) + '\b', '<USER>', 'IgnoreCase')
    }
    if ($env:COMPUTERNAME -and $env:COMPUTERNAME.Length -ge 3) {
        $Text = [regex]::Replace($Text, '\b' + [regex]::Escape($env:COMPUTERNAME) + '\b', '<HOST>', 'IgnoreCase')
    }
    # Defence in depth: never let something shaped like a credential out.
    $Text = [regex]::Replace($Text, '(?i)discord(app)?\.com/api/webhooks/\S+', '<REDACTED-WEBHOOK>')
    $Text = [regex]::Replace($Text, '\b(gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b', '<REDACTED-TOKEN>')
    return $Text
}

function Get-LogLines([string]$Path, [switch]$DropTitleOutput) {
    $lines = [System.IO.File]::ReadAllLines($Path)
    $kept = New-Object System.Collections.Generic.List[string]
    $dropped = 0
    foreach ($l in $lines) {
        $clean = [regex]::Replace($l, '\x1b\[[0-9;]*[A-Za-z]', '')
        if ($DropTitleOutput -and $clean -match '^\S*\s*(TRACE|DEBUG|INFO|WARN|ERROR)\s+title:') {
            $dropped++
            continue
        }
        $kept.Add($l)
    }
    $truncated = 0
    if ($kept.Count -gt $MaxLogLines) {
        $truncated = $kept.Count - $MaxLogLines
        $kept = $kept.GetRange($kept.Count - $MaxLogLines, $MaxLogLines)
    }
    $header = "# sanitized by Export-DiagnosticArtifacts.ps1: title output lines dropped=$dropped, leading lines truncated=$truncated"
    return ,(@($header) + $kept.ToArray())
}

function Write-Sanitized([string]$Name, [string]$Text) {
    $Text = Protect-Text $Text
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
    if ($bytes.LongLength -gt $MaxFileBytes) {
        $keep = [int]$MaxFileBytes
        $Text = "# truncated to the last $MaxFileBytes bytes`n" + [System.Text.Encoding]::UTF8.GetString($bytes, $bytes.Length - $keep, $keep)
    }
    [System.IO.File]::WriteAllText((Join-Path $Destination $Name), $Text, (New-Object System.Text.UTF8Encoding($false)))
}

$exported = New-Object System.Collections.Generic.List[string]
foreach ($name in @('summary.json', 'report.md', 'samples.csv')) {
    $src = Join-Path $RunDirectory $name
    if (Test-Path -LiteralPath $src) {
        Write-Sanitized $name ([System.IO.File]::ReadAllText($src))
        $exported.Add($name)
    }
}
foreach ($pair in @(@('stderr.log', 'stderr-filtered.log'), @('stdout.log', 'stdout-filtered.log'))) {
    $src = Join-Path $RunDirectory $pair[0]
    if (Test-Path -LiteralPath $src) {
        Write-Sanitized $pair[1] ((Get-LogLines $src -DropTitleOutput) -join "`n")
        $exported.Add($pair[1])
    }
}
foreach ($f in @(Get-ChildItem -LiteralPath $RunDirectory -File | Where-Object { $_.Name -like 'memory-*.log' -or $_.Name -like '*-crashes.log' })) {
    Write-Sanitized $f.Name ((Get-LogLines $f.FullName) -join "`n")
    $exported.Add($f.Name)
}

# Verify the bundle: text only, allowlisted types, bounded size.
$allowed = @('.json', '.md', '.csv', '.log', '.txt')
$total = 0L
$manifest = New-Object System.Collections.Generic.List[object]
foreach ($f in @(Get-ChildItem -LiteralPath $Destination -Recurse -File)) {
    if ($allowed -notcontains $f.Extension.ToLowerInvariant()) { throw "Not an allowed bundle file type: $($f.Name)" }
    $head = New-Object byte[] 4
    $fs = [System.IO.File]::OpenRead($f.FullName)
    try { $n = $fs.Read($head, 0, 4) } finally { $fs.Dispose() }
    if ($n -ge 2 -and $head[0] -eq 0x4D -and $head[1] -eq 0x5A) { throw "Executable header in bundle file: $($f.Name)" }
    if ([Array]::IndexOf([System.IO.File]::ReadAllBytes($f.FullName), [byte]0) -ge 0) { throw "Binary data in bundle file: $($f.Name)" }
    $total += $f.Length
    $manifest.Add([ordered]@{ File = $f.Name; Bytes = $f.Length; Sha256 = (Get-FileHash -LiteralPath $f.FullName -Algorithm SHA256).Hash })
}
if ($total -gt $MaxBundleBytes) { throw "Sanitized bundle is $total bytes (limit $MaxBundleBytes)." }

$note = [ordered]@{
    Note  = 'Sanitized subset of a local HUST-VIctory diagnostic run. Frames, executables, title files and title output stay on the home machine.'
    Files = $manifest.ToArray()
}
$note | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Destination 'manifest.json') -Encoding UTF8
Write-Host "sanitized bundle: $($manifest.Count) file(s), $total bytes -> $Destination"
