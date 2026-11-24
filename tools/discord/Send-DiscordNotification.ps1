#Requires -Version 5.1
<#
.SYNOPSIS
    Send an event-driven status notification to the locally configured Discord webhook.
.DESCRIPTION
    Decrypts the user-scoped Windows DPAPI webhook stored at
    %LOCALAPPDATA%\HUST-VIctory\discord-webhook.dpapi, posts a structured embed,
    handles Discord 429 rate limits and transient errors gracefully, and never
    logs or echoes the webhook secret.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('Commit', 'TestSuite', 'Upstream', 'Crash', 'GitPush', 'Checkpoint', 'Authorization', 'Diagnostic', 'Quota', 'Test')]
    [string]$Category,

    [Parameter(Mandatory)]
    [string]$Description,

    [ValidateSet('Success', 'Failure', 'Warning', 'Info')]
    [string]$Status = 'Info',

    [string]$NextAction = '',
    [string]$Project = 'HUST-VIctory',
    [string]$Branch = '',
    [string]$Commit = '',
    [string]$RepoRoot = '',
    [int]$MaxRetries = 3,
    [switch]$DryRun,
    [switch]$Quiet
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Security

$SecretPath = Join-Path (Join-Path $env:LOCALAPPDATA 'HUST-VIctory') 'discord-webhook.dpapi'
$Entropy = [System.Text.Encoding]::UTF8.GetBytes('HUST-VIctory.DiscordWebhook.v1')

function Resolve-GitMetadata {
    param([string]$Root)
    $meta = [ordered]@{ Branch = 'unknown'; Commit = 'unknown' }
    try {
        $gitArgs = @()
        if ($Root) { $gitArgs += @('-C', $Root) }
        $b = & git @gitArgs rev-parse --abbrev-ref HEAD 2>$null
        if ($LASTEXITCODE -eq 0 -and $b) { $meta.Branch = ($b | Select-Object -First 1).Trim() }
        $c = & git @gitArgs rev-parse --short HEAD 2>$null
        if ($LASTEXITCODE -eq 0 -and $c) { $meta.Commit = ($c | Select-Object -First 1).Trim() }
    } catch {
        # Best effort only.
    }
    return $meta
}

function Get-StatusColor {
    param([string]$S)
    switch ($S) {
        'Success' { return 3066993 }  # Green
        'Failure' { return 15158332 } # Red
        'Warning' { return 15105570 } # Orange
        default   { return 3447003 }  # Blue
    }
}

function Read-WebhookUrl {
    if (-not (Test-Path -LiteralPath $SecretPath)) {
        return $null
    }
    $cipher = [System.IO.File]::ReadAllBytes($SecretPath)
    $plainBytes = [System.Security.Cryptography.ProtectedData]::Unprotect(
        $cipher,
        $Entropy,
        [System.Security.Cryptography.DataProtectionScope]::CurrentUser
    )
    try {
        $url = [System.Text.Encoding]::UTF8.GetString($plainBytes).Trim()
        if ($url -notmatch '^https://(ptb\.|canary\.)?(discord|discordapp)\.com/api/webhooks/\d+/[A-Za-z0-9_\-]+$') {
            return $null
        }
        return $url
    } finally {
        [Array]::Clear($plainBytes, 0, $plainBytes.Length)
    }
}

if (-not $RepoRoot) {
    $candidate = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    if (Test-Path -LiteralPath (Join-Path $candidate '.git')) {
        $RepoRoot = $candidate
    }
}

if (-not $Branch -or -not $Commit) {
    $gitMeta = Resolve-GitMetadata -Root $RepoRoot
    if (-not $Branch) { $Branch = $gitMeta.Branch }
    if (-not $Commit) { $Commit = $gitMeta.Commit }
}

$nowUtc = [DateTime]::UtcNow
$timestampIso = $nowUtc.ToString('yyyy-MM-ddTHH:mm:ssZ')
$timestampLocal = (Get-Date).ToString('yyyy-MM-dd HH:mm:ss zzz')

$fields = @(
    @{ name = 'Project';  value = $Project; inline = $true }
    @{ name = 'Status';   value = $Status; inline = $true }
    @{ name = 'Category'; value = $Category; inline = $true }
    @{ name = 'Branch';   value = ('`' + $Branch + '`'); inline = $true }
    @{ name = 'Commit';   value = ('`' + $Commit + '`'); inline = $true }
    @{ name = 'Local Time'; value = $timestampLocal; inline = $true }
)

if (-not [string]::IsNullOrWhiteSpace($NextAction)) {
    $fields += @{ name = 'Next Action'; value = $NextAction; inline = $false }
}

$safeDesc = if ($Description.Length -gt 1800) {
    $Description.Substring(0, 1797) + '...'
} else {
    $Description
}

$payloadObj = @{
    username = 'HUST-VIctory Agent'
    embeds   = @(
        @{
            title       = "[$Project] $Category — $Status"
            description = $safeDesc
            color       = (Get-StatusColor -S $Status)
            timestamp   = $timestampIso
            fields      = $fields
            footer      = @{ text = 'Event-driven notification (active session only)' }
        }
    )
}

$jsonBody = $payloadObj | ConvertTo-Json -Depth 6 -Compress

if ($DryRun) {
    [PSCustomObject]@{
        Sent       = $false
        DryRun     = $true
        Configured = (Test-Path -LiteralPath $SecretPath)
        Category   = $Category
        Status     = $Status
        Branch     = $Branch
        Commit     = $Commit
        Payload    = $jsonBody
    }
    exit 0
}

$webhookUrl = $null
try {
    $webhookUrl = Read-WebhookUrl
} catch {
    if (-not $Quiet) {
        Write-Warning "Discord notification skipped: failed to decrypt DPAPI credential."
    }
    [PSCustomObject]@{ Sent = $false; Reason = 'DecryptFailed'; StatusCode = 0 }
    exit 0
}

if (-not $webhookUrl) {
    if (-not $Quiet) {
        Write-Host "Discord notification skipped: webhook not configured (run tools\discord\Setup-DiscordWebhook.ps1)."
    }
    [PSCustomObject]@{ Sent = $false; Reason = 'NotConfigured'; StatusCode = 0 }
    exit 0
}

$attempt = 0
$sent = $false
$lastReason = 'UnknownError'
$lastCode = 0

try {
    while ($attempt -lt $MaxRetries -and -not $sent) {
        $attempt++
        try {
            $resp = Invoke-WebRequest `
                -Uri $webhookUrl `
                -Method Post `
                -ContentType 'application/json; charset=utf-8' `
                -Body ([System.Text.Encoding]::UTF8.GetBytes($jsonBody)) `
                -TimeoutSec 15 `
                -UseBasicParsing
            $lastCode = [int]$resp.StatusCode
            if ($lastCode -ge 200 -and $lastCode -lt 300) {
                $sent = $true
                $lastReason = 'OK'
                break
            }
            $lastReason = "HTTP_$lastCode"
        } catch {
            $ex = $_.Exception
            $code = 0
            $retrySec = 0.0
            if ($null -ne $ex.Response) {
                try { $code = [int]$ex.Response.StatusCode } catch {}
                try {
                    $hdr = $ex.Response.Headers['Retry-After']
                    if ($hdr) { [double]::TryParse($hdr, [ref]$retrySec) | Out-Null }
                } catch {}
            }
            $lastCode = $code
            if ($code -eq 429) {
                $lastReason = 'RateLimited'
                if ($retrySec -le 0) { $retrySec = 2.0 * $attempt }
                if ($retrySec -gt 30) { $retrySec = 30.0 }
                Start-Sleep -Milliseconds ([int]($retrySec * 1000))
                continue
            }
            if ($code -ge 500 -and $attempt -lt $MaxRetries) {
                $lastReason = "ServerError_$code"
                Start-Sleep -Seconds (2 * $attempt)
                continue
            }
            $lastReason = if ($code -gt 0) { "HTTP_$code" } else { 'NetworkError' }
            break
        }
    }
} finally {
    $webhookUrl = $null
}

if (-not $sent -and -not $Quiet) {
    Write-Warning "Discord notification could not be delivered ($lastReason, HTTP $lastCode). Continuing without failing task."
}

[PSCustomObject]@{
    Sent       = $sent
    Reason     = $lastReason
    StatusCode = $lastCode
    Category   = $Category
    Status     = $Status
}
