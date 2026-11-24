#Requires -Version 5.1
<#
.SYNOPSIS
    Securely configure, inspect, test, or remove the local HUST-VIctory Discord webhook.
.DESCRIPTION
    Prompts interactively with Read-Host -AsSecureString so the webhook URL is never
    placed in command-line arguments, PowerShell history, AI chat logs, or the Git repo.
    Encrypts the URL using Windows DPAPI (CurrentUser scope) and stores the ciphertext
    under %LOCALAPPDATA%\HUST-VIctory\discord-webhook.dpapi.
#>
[CmdletBinding()]
param(
    [switch]$Status,
    [switch]$Remove,
    [switch]$Test
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

Add-Type -AssemblyName System.Security

$SecretDir = Join-Path $env:LOCALAPPDATA 'HUST-VIctory'
$SecretPath = Join-Path $SecretDir 'discord-webhook.dpapi'
$Entropy = [System.Text.Encoding]::UTF8.GetBytes('HUST-VIctory.DiscordWebhook.v1')

function ConvertFrom-SecureStringPlain {
    param([Parameter(Mandatory)][System.Security.SecureString]$Secure)
    $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($Secure)
    try {
        return [System.Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
    } finally {
        [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    }
}

if ($Remove) {
    if (Test-Path -LiteralPath $SecretPath) {
        Remove-Item -LiteralPath $SecretPath -Force
        Write-Host "Removed stored Discord webhook credential from $SecretPath."
    } else {
        Write-Host "No stored Discord webhook credential found."
    }
    exit 0
}

if ($Status) {
    if (-not (Test-Path -LiteralPath $SecretPath)) {
        Write-Host "Discord webhook status: NOT CONFIGURED ($SecretPath missing)."
        exit 0
    }
    try {
        $cipher = [System.IO.File]::ReadAllBytes($SecretPath)
        $plainBytes = [System.Security.Cryptography.ProtectedData]::Unprotect(
            $cipher,
            $Entropy,
            [System.Security.Cryptography.DataProtectionScope]::CurrentUser
        )
        $plain = [System.Text.Encoding]::UTF8.GetString($plainBytes)
        [Array]::Clear($plainBytes, 0, $plainBytes.Length)
        $valid = $plain -match '^https://(ptb\.|canary\.)?(discord|discordapp)\.com/api/webhooks/\d+/[A-Za-z0-9_\-]+$'
        $plain = $null
        if ($valid) {
            Write-Host "Discord webhook status: CONFIGURED (DPAPI CurrentUser, valid webhook format)."
        } else {
            Write-Warning "Discord webhook status: STORED BUT INVALID FORMAT. Re-run Setup-DiscordWebhook.ps1."
        }
    } catch {
        Write-Warning "Discord webhook status: UNREADABLE (DPAPI decrypt failed for current user)."
    }
    exit 0
}

Write-Host "=== HUST-VIctory Local Discord Webhook Setup ==="
Write-Host "Your webhook URL will be read securely (masked), encrypted with Windows DPAPI"
Write-Host "(CurrentUser scope), and stored outside the repository at:"
Write-Host "  $SecretPath"
Write-Host "It will NOT be written to Git, shell history, or log files."
Write-Host ""

$secureInput = Read-Host -Prompt "Paste Discord incoming webhook URL" -AsSecureString
$url = $null
$urlBytes = $null
try {
    $url = (ConvertFrom-SecureStringPlain -Secure $secureInput).Trim()
    if ([string]::IsNullOrWhiteSpace($url)) {
        throw "No webhook URL entered. Setup cancelled."
    }
    if ($url -notmatch '^https://(ptb\.|canary\.)?(discord|discordapp)\.com/api/webhooks/\d+/[A-Za-z0-9_\-]+$') {
        throw "Invalid Discord webhook URL format. Expected https://discord.com/api/webhooks/<id>/<token>."
    }

    if (-not (Test-Path -LiteralPath $SecretDir)) {
        New-Item -ItemType Directory -Path $SecretDir -Force | Out-Null
    }

    $urlBytes = [System.Text.Encoding]::UTF8.GetBytes($url)
    $cipherBytes = [System.Security.Cryptography.ProtectedData]::Protect(
        $urlBytes,
        $Entropy,
        [System.Security.Cryptography.DataProtectionScope]::CurrentUser
    )
    [System.IO.File]::WriteAllBytes($SecretPath, $cipherBytes)
    Write-Host "Webhook saved securely with Windows DPAPI (CurrentUser)."
} finally {
    if ($null -ne $urlBytes) {
        [Array]::Clear($urlBytes, 0, $urlBytes.Length)
    }
    $url = $null
    if ($null -ne $secureInput) {
        $secureInput.Dispose()
    }
}

if ($Test) {
    $sender = Join-Path $PSScriptRoot 'Send-DiscordNotification.ps1'
    & $sender `
        -Category 'Test' `
        -Status 'Success' `
        -Description 'HUST-VIctory local DPAPI Discord webhook setup verified.' `
        -NextAction 'Event-driven notifications are now ready for commits, full test runs, pushes, and checkpoints.'
}
