# HUST-VIctory Local Discord Notifications

Event-driven Discord webhook notifications backed by Windows DPAPI (`CurrentUser` scope).

## Security Model

- The webhook URL is **never** stored in the repository, passed as a CLI argument, logged, or echoed.
- `Setup-DiscordWebhook.ps1` reads the URL interactively with `Read-Host -AsSecureString` (masked, kept out of shell history) and encrypts it via `System.Security.Cryptography.ProtectedData` (`DataProtectionScope::CurrentUser`).
- Ciphertext is stored outside the Git repository at:
  `%LOCALAPPDATA%\HUST-VIctory\discord-webhook.dpapi`

## One-Time Interactive Setup

Run this locally in your PowerShell terminal (never paste the webhook URL into chat):

```powershell
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\discord\Setup-DiscordWebhook.ps1 -Test
```

Other management commands:

```powershell
# Verify whether a DPAPI credential is configured (never prints the URL)
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\discord\Setup-DiscordWebhook.ps1 -Status

# Remove the stored credential
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\discord\Setup-DiscordWebhook.ps1 -Remove
```

## Sending Notifications

```powershell
powershell -ExecutionPolicy Bypass -File G:\VIctory\tools\discord\Send-DiscordNotification.ps1 `
    -Category Checkpoint `
    -Status Success `
    -Description "Phase 13 checkpoint completed." `
    -NextAction "Review upstream comparison report."
```

## Notification Policy

Notifications are strictly event-driven during active engineering or diagnostic runs (no background daemon runs while an agent session is inactive). Send notifications only for:

1. Significant engineering fix committed (`Commit`)
2. Full test suite pass or failure (`TestSuite`)
3. Important upstream fix integrated (`Upstream`)
4. Crash or critical regression diagnosed (`Crash`)
5. Private GitHub push succeeded (`GitPush`)
6. Development checkpoint completed (`Checkpoint`)
7. Next step requires user authorization (`Authorization`)
8. Controlled diagnostic harness completion (`Diagnostic`)

Missing credentials, API errors, and Discord `429` rate limits are handled gracefully without failing builds, tests, or diagnostic runs.
